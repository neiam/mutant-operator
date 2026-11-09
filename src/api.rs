//! HTTP API for Mutations: base Deployments whose variants are configured at
//! runtime.
//!
//! ```text
//! GET    /healthz
//! GET    /api/v1/namespaces/{ns}/mutations
//! POST   /api/v1/namespaces/{ns}/mutations                   {name, deployment, variants}
//! GET    /api/v1/namespaces/{ns}/mutations/{name}
//! DELETE /api/v1/namespaces/{ns}/mutations/{name}
//! GET    /api/v1/namespaces/{ns}/mutations/{name}/variants
//! POST   /api/v1/namespaces/{ns}/mutations/{name}/variants   {label, env, instances}
//! GET    /api/v1/namespaces/{ns}/mutations/{name}/variants/{label}
//! DELETE /api/v1/namespaces/{ns}/mutations/{name}/variants/{label}
//! ```
//!
//! ## Authorization
//!
//! Requests carry `Authorization: Bearer <token>`. A request about one
//! Mutation -- getting it, listing, adding or removing its variants -- is
//! allowed with that Mutation's own token: the `token` in the
//! `<name>-secret` the operator generated for it, in its namespace. An app
//! reads that Secret from its own namespace and can then manage that one
//! Mutation's variants, and nothing else. Listing every Mutation in a
//! namespace, and creating or deleting a Mutation, take the admin token
//! (`MUTANT_API_TOKEN`), as does anything when no Mutation token applies.
//!
//! A Mutation is a Kubernetes object, so the operator keeps no state of its
//! own. One synced by Argo CD cannot be deleted here, since the next sync
//! would put it back, and its variants are `readOnly` too unless it carries
//! `mutantops.neiam.org/runtime-variants: "true"`: git holds the base, the
//! API the variants.

use crate::crd::{
    insert_variant, is_gitops_managed, remove_variant, secret_name, validate_base, validate_label,
    validate_variants, variants_editable, SECRET_TOKEN_KEY,
    Mutation, MutationSpec, Variant, VariantState, VariantStatus,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Secret;
use kube::{
    api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams},
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn};

/// Attempts at a read-modify-write of a Mutation's variants before giving
/// up on conflicts.
const MAX_ATTEMPTS: usize = 5;

#[derive(Clone)]
pub struct AppState {
    client: Client,
    /// Admin token: any request, including listing, creating and deleting
    /// Mutations. `None`: only Mutation tokens are accepted.
    admin_token: Option<Arc<str>>,
    /// Accept every request unauthenticated. For local development only.
    insecure: bool,
}

impl AppState {
    pub fn new(client: Client, admin_token: Option<String>, insecure: bool) -> Self {
        Self { client, admin_token: admin_token.map(Into::into), insecure }
    }

    fn mutations(&self, ns: &str) -> Api<Mutation> {
        Api::namespaced(self.client.clone(), ns)
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/namespaces/{ns}/mutations", get(list_mutations).post(create_mutation))
        .route("/namespaces/{ns}/mutations/{name}", get(get_mutation).delete(delete_mutation))
        .route(
            "/namespaces/{ns}/mutations/{name}/variants",
            get(list_variants).post(create_variant),
        )
        .route(
            "/namespaces/{ns}/mutations/{name}/variants/{label}",
            get(get_variant).delete(delete_variant),
        );
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api/v1", api)
        .with_state(state)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Whether `presented` is the admin token (or auth is off).
fn admin_ok(admin_token: Option<&str>, insecure: bool, presented: Option<&str>) -> bool {
    insecure
        || match (admin_token, presented) {
            (Some(want), Some(got)) => constant_time_eq(want.as_bytes(), got.as_bytes()),
            _ => false,
        }
}

/// Whether `secret` is the Mutation's own -- owned by the Mutation with
/// `uid`, so not a look-alike someone else created -- and holds `presented`
/// as its token.
fn secret_grants(secret: &Secret, uid: &str, presented: &str) -> bool {
    let owned = secret.owner_references().iter().any(|o| o.uid == uid);
    let token = secret.data.as_ref().and_then(|d| d.get(SECRET_TOKEN_KEY));
    owned && token.is_some_and(|t| constant_time_eq(&t.0, presented.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Allows only the admin token.
fn require_admin(st: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if admin_ok(st.admin_token.as_deref(), st.insecure, bearer(headers)) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

/// Allows the admin token, or the Mutation's own token, for a request about
/// Mutation `name` in `ns`. Returns the Mutation.
///
/// Without the admin token, a Mutation that does not exist answers the same
/// as a wrong token, so one Mutation's token cannot probe for others.
async fn authorize_mutation(st: &AppState, headers: &HeaderMap, ns: &str, name: &str) -> ApiResult<Mutation> {
    let presented = bearer(headers);
    if admin_ok(st.admin_token.as_deref(), st.insecure, presented) {
        return Ok(st.mutations(ns).get(name).await?);
    }
    let presented = presented.ok_or(ApiError::Unauthorized)?;
    let m = st.mutations(ns).get_opt(name).await?.ok_or(ApiError::Unauthorized)?;
    let uid = m.uid().ok_or(ApiError::Unauthorized)?;
    let secret = Api::<Secret>::namespaced(st.client.clone(), ns).get_opt(&secret_name(name)).await?;
    if secret.is_some_and(|s| secret_grants(&s, &uid, presented)) {
        Ok(m)
    } else {
        Err(ApiError::Unauthorized)
    }
}

#[derive(Debug)]
enum ApiError {
    Unauthorized,
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Kube(kube::Error),
}

impl From<kube::Error> for ApiError {
    fn from(err: kube::Error) -> Self {
        match err {
            kube::Error::Api(ref e) if e.code == 404 => ApiError::NotFound(e.message.clone()),
            kube::Error::Api(ref e) if e.code == 409 => ApiError::Conflict(e.message.clone()),
            kube::Error::Api(ref e) if e.code == 422 => ApiError::BadRequest(e.message.clone()),
            other => ApiError::Kube(other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token: use this Mutation's token, or the admin token".into(),
            ),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Kube(e) => {
                warn!("kubernetes request failed: {e}");
                (StatusCode::BAD_GATEWAY, format!("kubernetes request failed: {e}"))
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// A Mutation as the API reports it.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MutationView {
    name: String,
    namespace: String,
    /// The base Deployment.
    deployment: Deployment,
    variants: Vec<VariantView>,
    /// The generated Secret, once the operator has made it.
    secret: Option<String>,
    summary: Option<String>,
    message: Option<String>,
    /// Synced by Argo CD: the Mutation itself changes in git, not here.
    read_only: bool,
    /// Whether variants can be added and removed here.
    variants_editable: bool,
}

/// A variant: its definition and its last reported state.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct VariantView {
    #[serde(flatten)]
    variant: Variant,
    status: VariantStatus,
    read_only: bool,
}

#[derive(Deserialize)]
struct CreateMutation {
    name: String,
    deployment: Deployment,
    #[serde(default)]
    variants: Vec<Variant>,
}

impl From<Mutation> for MutationView {
    fn from(m: Mutation) -> Self {
        let read_only = is_gitops_managed(&m);
        let editable = variants_editable(&m);
        let status = m.status.clone().unwrap_or_default();
        let variants = m
            .spec
            .variants
            .iter()
            .map(|v| VariantView {
                status: status.variants.iter().find(|s| s.label == v.label).cloned().unwrap_or_else(|| {
                    // Added since the operator last reported.
                    VariantStatus::rejected(&v.label, VariantState::Progressing, "not yet reconciled".into())
                }),
                variant: v.clone(),
                read_only: !editable,
            })
            .collect();
        MutationView {
            name: m.name_any(),
            namespace: m.namespace().unwrap_or_default(),
            deployment: m.spec.deployment,
            variants,
            secret: status.secret,
            summary: status.summary,
            message: status.message,
            read_only,
            variants_editable: editable,
        }
    }
}

async fn list_mutations(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<MutationView>>> {
    require_admin(&st, &headers)?;
    let items = st.mutations(&ns).list(&ListParams::default()).await?;
    Ok(Json(items.into_iter().map(Into::into).collect()))
}

async fn get_mutation(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<Json<MutationView>> {
    Ok(Json(authorize_mutation(&st, &headers, &ns, &name).await?.into()))
}

async fn create_mutation(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(ns): Path<String>,
    Json(req): Json<CreateMutation>,
) -> ApiResult<(StatusCode, Json<MutationView>)> {
    require_admin(&st, &headers)?;
    validate_label(&req.name).map_err(ApiError::BadRequest)?;
    validate_base(&req.deployment).map_err(ApiError::BadRequest)?;
    validate_variants(&req.variants).map_err(ApiError::BadRequest)?;
    let mutation = Mutation::new(&req.name, MutationSpec { deployment: req.deployment, variants: req.variants });
    let created = st.mutations(&ns).create(&PostParams::default(), &mutation).await?;
    info!(%ns, name = %req.name, "mutation created");
    Ok((StatusCode::CREATED, Json(created.into())))
}

async fn delete_mutation(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    require_admin(&st, &headers)?;
    let api = st.mutations(&ns);
    refuse_if_gitops(&api.get(&name).await?)?;
    api.delete(&name, &DeleteParams::default()).await?;
    info!(%ns, %name, "mutation deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn list_variants(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<VariantView>>> {
    Ok(Json(MutationView::from(authorize_mutation(&st, &headers, &ns, &name).await?).variants))
}

async fn get_variant(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((ns, name, label)): Path<(String, String, String)>,
) -> ApiResult<Json<VariantView>> {
    MutationView::from(authorize_mutation(&st, &headers, &ns, &name).await?)
        .variants
        .into_iter()
        .find(|v| v.variant.label == label)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("variant {label:?} not found")))
}

async fn create_variant(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((ns, name)): Path<(String, String)>,
    Json(variant): Json<Variant>,
) -> ApiResult<(StatusCode, Json<Variant>)> {
    authorize_mutation(&st, &headers, &ns, &name).await?;
    variant.validate().map_err(ApiError::BadRequest)?;
    let created = variant.clone();
    update_variants(&st.mutations(&ns), &name, |list| {
        insert_variant(list, variant.clone()).map_err(ApiError::Conflict)
    })
    .await?;
    info!(%ns, %name, label = %created.label, "variant created");
    Ok((StatusCode::CREATED, Json(created)))
}

async fn delete_variant(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((ns, name, label)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    authorize_mutation(&st, &headers, &ns, &name).await?;
    update_variants(&st.mutations(&ns), &name, |list| {
        remove_variant(list, &label)
            .map(drop)
            .ok_or_else(|| ApiError::NotFound(format!("variant {label:?} not found")))
    })
    .await?;
    info!(%ns, %name, %label, "variant deleted");
    Ok(StatusCode::NO_CONTENT)
}

fn refuse_if_gitops(m: &Mutation) -> ApiResult<()> {
    if is_gitops_managed(m) {
        return Err(ApiError::Conflict(format!(
            "mutation {} is synced by Argo CD; change it in git instead",
            m.name_any()
        )));
    }
    Ok(())
}

/// Read-modify-write of `spec.variants`, guarded by `resourceVersion` so a
/// concurrent edit (another API call, or kubectl) is never overwritten: the
/// API server answers 409 when the object changed since the read, and the
/// edit is retried against the fresh copy.
async fn update_variants<F>(api: &Api<Mutation>, name: &str, edit: F) -> ApiResult<()>
where
    F: Fn(&mut Vec<Variant>) -> ApiResult<()>,
{
    for _ in 0..MAX_ATTEMPTS {
        let current = api.get(name).await?;
        if !variants_editable(&current) {
            return Err(ApiError::Conflict(format!(
                "mutation {name} is synced by Argo CD without {}: \"true\"; change its variants in git instead",
                crate::crd::RUNTIME_VARIANTS_ANNOTATION
            )));
        }
        let mut variants = current.spec.variants.clone();
        edit(&mut variants)?;
        let patch = json!({
            "metadata": { "resourceVersion": current.resource_version() },
            "spec": { "variants": variants },
        });
        match api.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => return Ok(()),
            Err(kube::Error::Api(e)) if e.code == 409 => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::Conflict(format!("{name} kept changing; gave up after {MAX_ATTEMPTS} attempts")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::MutationStatus;
    use std::collections::HashMap;

    #[test]
    fn admin_auth() {
        assert!(admin_ok(Some("s3cret"), false, Some("s3cret")));
        assert!(!admin_ok(Some("s3cret"), false, Some("s3cre")));
        assert!(!admin_ok(Some("s3cret"), false, Some("s3creT")));
        assert!(!admin_ok(Some("s3cret"), false, None));
        // No admin token configured: nothing is admin, unless auth is off.
        assert!(!admin_ok(None, false, Some("anything")));
        assert!(!admin_ok(None, false, None));
        assert!(admin_ok(None, true, None));
    }

    #[test]
    fn mutation_token_auth() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let m = {
            let mut m = Mutation::new("browser-snap", MutationSpec { deployment: Deployment::default(), variants: vec![] });
            m.metadata.uid = Some("uid-1".into());
            m.metadata.namespace = Some("crib".into());
            m
        };
        let owner = OwnerReference { uid: "uid-1".into(), ..Default::default() };
        let secret = crate::crd::build_secret(&m, owner, "tok".into());

        assert!(secret_grants(&secret, "uid-1", "tok"));
        assert!(!secret_grants(&secret, "uid-1", "tok2"));
        assert!(!secret_grants(&secret, "uid-1", ""));
        // The same name and token, but not owned by this Mutation.
        assert!(!secret_grants(&secret, "uid-2", "tok"));
        let mut tokenless = secret.clone();
        tokenless.data = None;
        assert!(!secret_grants(&tokenless, "uid-1", "tok"));
    }

    #[test]
    fn view_joins_variants_with_status() {
        let v = |label: &str| Variant { label: label.into(), env: HashMap::new(), instances: 1 };
        let mut m = Mutation::new(
            "browser-snap",
            MutationSpec { deployment: Deployment::default(), variants: vec![v("lilygo47"), v("new")] },
        );
        m.status = Some(MutationStatus {
            secret: Some("browser-snap-secret".into()),
            variants: vec![VariantStatus { ready_replicas: Some(1), ..VariantStatus::rejected("lilygo47", VariantState::Ready, String::new()) }],
            ..Default::default()
        });
        let view = MutationView::from(m);
        assert_eq!(view.secret.as_deref(), Some("browser-snap-secret"));
        assert!(!view.read_only);
        assert_eq!(view.variants[0].status.state, VariantState::Ready);
        assert_eq!(view.variants[1].status.message.as_deref(), Some("not yet reconciled"));
    }
}
