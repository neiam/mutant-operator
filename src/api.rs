//! HTTP API for listing, creating and deleting the mutations of a live
//! MutantDeployment.
//!
//! Created mutations are standalone `Mutation` resources, never edits to the
//! MutantDeployment, which can stay owned by Argo CD (or any GitOps tool)
//! without the API and the sync fighting over it. Mutations declared inline
//! in the MutantDeployment are listed too, but are read-only here: they
//! change in git.
//!
//! ```text
//! GET    /healthz
//! GET    /api/v1/namespaces/{ns}/mutantdeployments
//! GET    /api/v1/namespaces/{ns}/mutantdeployments/{name}/mutations
//! POST   /api/v1/namespaces/{ns}/mutantdeployments/{name}/mutations
//! GET    /api/v1/namespaces/{ns}/mutantdeployments/{name}/mutations/{label}
//! DELETE /api/v1/namespaces/{ns}/mutantdeployments/{name}/mutations/{label}
//! ```

use crate::crd::{
    applied_status, resolve, MutantDeployment, Mutation, MutationDef, MutationSpec, MutationStatus,
    Source, OWNER_LABEL, TARGET_LABEL,
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
    api::{Api, DeleteParams, ListParams, PostParams},
    Client, Resource, ResourceExt,
};
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use tracing::{info, warn};

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
        .route("/namespaces/{ns}/mutantdeployments", get(list_mutants))
        .route(
            "/namespaces/{ns}/mutantdeployments/{name}/mutations",
            get(list_mutations).post(create_mutation),
        )
        .route(
            "/namespaces/{ns}/mutantdeployments/{name}/mutations/{label}",
            get(get_mutation).delete(delete_mutation),
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

/// A mutation as the API reports it: its definition, where it was declared,
/// and its live state.
#[derive(Serialize, Debug)]
struct MutationView {
    #[serde(flatten)]
    def: MutationDef,
    source: Source,
    status: MutationStatus,
}

#[derive(Serialize)]
struct MutantSummary {
    name: String,
    deployment: String,
    mutations: Vec<String>,
}

/// Every mutation of `name`: the ones running, then Mutation resources that
/// are not (invalid, or a duplicate label), with why.
async fn views(client: &Client, ns: &str, name: &str) -> ApiResult<Vec<MutationView>> {
    let mutant = Api::<MutantDeployment>::namespaced(client.clone(), ns).get(name).await?;
    let resources = Api::<Mutation>::namespaced(client.clone(), ns)
        .list(&ListParams::default())
        .await?
        .items;
    // Status is read live from the Deployments rather than from Mutation
    // status, so inline mutations, which have no status, get one too.
    let deployments: BTreeMap<String, Deployment> = Api::<Deployment>::namespaced(client.clone(), ns)
        .list(&ListParams::default().labels(&format!("{OWNER_LABEL}={name}")))
        .await?
        .into_iter()
        .map(|d| (d.name_any(), d))
        .collect();

    let resolved = resolve(&mutant, &resources);
    let mut out: Vec<_> = resolved
        .active
        .into_iter()
        .map(|(def, source)| {
            let dname = mutant.deployment_name(&def.label);
            let status = applied_status(dname.clone(), def.instances, deployments.get(&dname));
            MutationView { def, source, status }
        })
        .collect();
    for (rname, state, message) in resolved.rejected {
        if let Some(m) = resources.iter().find(|m| m.name_any() == rname) {
            out.push(MutationView {
                def: m.spec.def.clone(),
                source: Source::Resource { name: rname },
                status: MutationStatus { state, message: Some(message), ..Default::default() },
            });
        }
    }
    Ok(out)
}

async fn list_mutants(State(st): State<AppState>, Path(ns): Path<String>) -> ApiResult<Json<Vec<MutantSummary>>> {
    let mutants = Api::<MutantDeployment>::namespaced(st.client.clone(), &ns)
        .list(&ListParams::default())
        .await?;
    let resources = Api::<Mutation>::namespaced(st.client, &ns)
        .list(&ListParams::default())
        .await?
        .items;
    Ok(Json(
        mutants
            .into_iter()
            .map(|md| MutantSummary {
                mutations: resolve(&md, &resources).active.into_iter().map(|(d, _)| d.label).collect(),
                deployment: md.base_name(),
                name: md.name_any(),
            })
            .collect(),
    ))
}

async fn list_mutations(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
) -> ApiResult<Json<Vec<MutationView>>> {
    Ok(Json(views(&st.client, &ns, &name).await?))
}

async fn get_mutation(
    State(st): State<AppState>,
    Path((ns, name, label)): Path<(String, String, String)>,
) -> ApiResult<Json<MutationView>> {
    // The running one: listed first, ahead of any rejected duplicate.
    views(&st.client, &ns, &name)
        .await?
        .into_iter()
        .find(|v| v.def.label == label)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("mutation {label:?} not found")))
}

/// Creates a Mutation resource named `{name}-{label}`.
async fn create_mutation(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    Json(def): Json<MutationDef>,
) -> ApiResult<(StatusCode, Json<Mutation>)> {
    def.validate().map_err(ApiError::BadRequest)?;
    let mutant = Api::<MutantDeployment>::namespaced(st.client.clone(), &ns).get(&name).await?;
    // Refused here rather than created only to sit in Conflict.
    if views(&st.client, &ns, &name).await?.iter().any(|v| v.def.label == def.label) {
        return Err(ApiError::Conflict(format!("mutation {:?} already exists", def.label)));
    }

    let label = def.label.clone();
    let mut mutation = Mutation::new(
        &format!("{name}-{label}"),
        MutationSpec { mutant_deployment: name.clone(), def },
    );
    mutation.metadata.labels = Some(BTreeMap::from([(TARGET_LABEL.to_string(), name.clone())]));
    // Set up front so the Mutation is a dependent of its MutantDeployment
    // from the start; the controller also adds it to hand-made ones.
    mutation.metadata.owner_references = mutant.owner_ref(&()).map(|o| vec![o]);

    let created = Api::<Mutation>::namespaced(st.client, &ns)
        .create(&PostParams::default(), &mutation)
        .await?;
    info!(%ns, %name, %label, "mutation created");
    Ok((StatusCode::CREATED, Json(created)))
}

/// Deletes every Mutation resource of `name` with this label: the running
/// one and any duplicate waiting behind it, which would otherwise take its
/// place. Inline mutations are refused.
async fn delete_mutation(
    State(st): State<AppState>,
    Path((ns, name, label)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let matching: Vec<_> = views(&st.client, &ns, &name)
        .await?
        .into_iter()
        .filter(|v| v.def.label == label)
        .collect();
    if matching.is_empty() {
        return Err(ApiError::NotFound(format!("mutation {label:?} not found")));
    }
    if matching.iter().any(|v| v.source == Source::Inline) {
        return Err(ApiError::Conflict(format!(
            "mutation {label:?} is declared inline in MutantDeployment {name}; remove it from its spec.mutations (in git) instead"
        )));
    }
    let api = Api::<Mutation>::namespaced(st.client, &ns);
    for v in matching {
        if let Source::Resource { name: rname } = v.source {
            match api.delete(&rname, &DeleteParams::default()).await {
                Ok(_) | Err(kube::Error::Api(kube::core::ErrorResponse { code: 404, .. })) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    info!(%ns, %name, %label, "mutation deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth() {
        assert!(authorized(None, None));
        assert!(authorized(None, Some("anything")));
        assert!(authorized(Some("s3cret"), Some("s3cret")));
        assert!(!authorized(Some("s3cret"), Some("s3cre")));
        assert!(!authorized(Some("s3cret"), Some("s3creT")));
        assert!(!authorized(Some("s3cret"), None));
    }
}
