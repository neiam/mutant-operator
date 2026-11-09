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
//! A Mutation is a Kubernetes object, so the operator keeps no state of its
//! own. Ones synced by Argo CD are listed with `readOnly: true` and cannot be
//! changed here: the next sync would put them back.

use crate::crd::{
    insert_variant, is_gitops_managed, remove_variant, validate_base, validate_label, validate_variants,
    Mutation, MutationSpec, Variant, VariantState, VariantStatus,
};
use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use k8s_openapi::api::apps::v1::Deployment;
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
    /// Bearer token required on every `/api` request; `None` disables auth.
    token: Option<Arc<str>>,
}

impl AppState {
    pub fn new(client: Client, token: Option<String>) -> Self {
        Self { client, token: token.map(Into::into) }
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
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api/v1", api)
        .with_state(state)
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if authorized(state.token.as_deref(), presented) {
        next.run(req).await
    } else {
        ApiError::Unauthorized.into_response()
    }
}

fn authorized(expected: Option<&str>, presented: Option<&str>) -> bool {
    match (expected, presented) {
        (None, _) => true,
        (Some(want), Some(got)) => constant_time_eq(want.as_bytes(), got.as_bytes()),
        (Some(_), None) => false,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token".into()),
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
    /// Synced by Argo CD: changes in git, not here.
    read_only: bool,
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
                read_only,
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
        }
    }
}

fn api(st: &AppState, ns: &str) -> Api<Mutation> {
    Api::namespaced(st.client.clone(), ns)
}

async fn list_mutations(State(st): State<AppState>, Path(ns): Path<String>) -> ApiResult<Json<Vec<MutationView>>> {
    let items = api(&st, &ns).list(&ListParams::default()).await?;
    Ok(Json(items.into_iter().map(Into::into).collect()))
}

async fn get_mutation(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<Json<MutationView>> {
    Ok(Json(api(&st, &ns).get(&name).await?.into()))
}

async fn create_mutation(
    State(st): State<AppState>,
    Path(ns): Path<String>,
    Json(req): Json<CreateMutation>,
) -> ApiResult<(StatusCode, Json<MutationView>)> {
    validate_label(&req.name).map_err(ApiError::BadRequest)?;
    validate_base(&req.deployment).map_err(ApiError::BadRequest)?;
    validate_variants(&req.variants).map_err(ApiError::BadRequest)?;
    let mutation = Mutation::new(&req.name, MutationSpec { deployment: req.deployment, variants: req.variants });
    let created = api(&st, &ns).create(&PostParams::default(), &mutation).await?;
    info!(%ns, name = %req.name, "mutation created");
    Ok((StatusCode::CREATED, Json(created.into())))
}

async fn delete_mutation(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let api = api(&st, &ns);
    refuse_if_gitops(&api.get(&name).await?)?;
    api.delete(&name, &DeleteParams::default()).await?;
    info!(%ns, %name, "mutation deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn list_variants(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<VariantView>>> {
    Ok(Json(MutationView::from(api(&st, &ns).get(&name).await?).variants))
}

async fn get_variant(
    State(st): State<AppState>,
    Path((ns, name, label)): Path<(String, String, String)>,
) -> ApiResult<Json<VariantView>> {
    MutationView::from(api(&st, &ns).get(&name).await?)
        .variants
        .into_iter()
        .find(|v| v.variant.label == label)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("variant {label:?} not found")))
}

async fn create_variant(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    Json(variant): Json<Variant>,
) -> ApiResult<(StatusCode, Json<Variant>)> {
    variant.validate().map_err(ApiError::BadRequest)?;
    let created = variant.clone();
    update_variants(&api(&st, &ns), &name, |list| {
        insert_variant(list, variant.clone()).map_err(ApiError::Conflict)
    })
    .await?;
    info!(%ns, %name, label = %created.label, "variant created");
    Ok((StatusCode::CREATED, Json(created)))
}

async fn delete_variant(
    State(st): State<AppState>,
    Path((ns, name, label)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    update_variants(&api(&st, &ns), &name, |list| {
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
        refuse_if_gitops(&current)?;
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
    fn auth() {
        assert!(authorized(None, None));
        assert!(authorized(None, Some("anything")));
        assert!(authorized(Some("s3cret"), Some("s3cret")));
        assert!(!authorized(Some("s3cret"), Some("s3cre")));
        assert!(!authorized(Some("s3cret"), Some("s3creT")));
        assert!(!authorized(Some("s3cret"), None));
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
