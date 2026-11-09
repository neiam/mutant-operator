//! Reconciles MutantDeployments and Mutations alike: a base Deployment plus
//! variants becomes one Deployment per variant, and Deployments whose
//! variant is gone are pruned. A Mutation also gets a generated Secret and a
//! status reporting each variant.

use crate::crd::{
    applied_status, build_deployment, build_secret, deployment_name, generate_token, secret_name, summarize,
    validate_base, MutantDeployment, Mutation, MutationStatus, Variant, VariantState, VariantStatus,
    MUTATION_LABEL, OWNER_LABEL,
};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{
    api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams},
    runtime::{
        controller::{Action, Controller},
        watcher,
    },
    Client, Resource, ResourceExt,
};
use serde_json::json;
use std::{collections::HashSet, fmt, sync::Arc, time::Duration};
use tracing::{info, warn};

const FIELD_MANAGER: &str = "mutantops-operator";
/// Periodic resync. Events drive reconciliation; this is a backstop.
const RESYNC: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub enum MutantopsError {
    Invalid(String),
    Kube(kube::Error),
}

impl std::error::Error for MutantopsError {}

impl fmt::Display for MutantopsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MutantopsError::Invalid(msg) => write!(f, "invalid resource: {msg}"),
            MutantopsError::Kube(err) => write!(f, "kube error: {err}"),
        }
    }
}

impl From<kube::Error> for MutantopsError {
    fn from(err: kube::Error) -> Self {
        MutantopsError::Kube(err)
    }
}

struct Ctx {
    client: Client,
}

/// Who the generated Deployments belong to.
struct Owner {
    name: String,
    ns: String,
    controller_ref: OwnerReference,
}

impl Owner {
    fn of<K: Resource<DynamicType = ()>>(obj: &K) -> Result<Self, MutantopsError> {
        Ok(Self {
            name: obj.name_any(),
            ns: obj
                .namespace()
                .ok_or_else(|| MutantopsError::Invalid(format!("{} has no namespace", obj.name_any())))?,
            controller_ref: obj
                .controller_owner_ref(&())
                .ok_or_else(|| MutantopsError::Invalid(format!("{} has no uid", obj.name_any())))?,
        })
    }
}

/// Applies one Deployment per variant of `base` and prunes the owner's
/// Deployments whose variant is gone. Returns each variant's status.
///
/// A variant is skipped, and reported, when it is invalid, repeats an
/// earlier label, or its Deployment name is already controlled by a
/// different owner: two owners would otherwise take turns overwriting it.
async fn apply_variants(
    client: &Client,
    owner: &Owner,
    base: &Deployment,
    variants: &[Variant],
) -> Result<Vec<VariantStatus>, kube::Error> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), &owner.ns);
    let apply = PatchParams::apply(FIELD_MANAGER).force();
    let uid = &owner.controller_ref.uid;

    let mut statuses = Vec::with_capacity(variants.len());
    let mut wanted = HashSet::new();
    let mut labels = HashSet::new();
    for v in variants {
        if let Err(msg) = v.validate() {
            statuses.push(VariantStatus::rejected(&v.label, VariantState::Invalid, msg));
            continue;
        }
        if !labels.insert(v.label.as_str()) {
            let msg = "label repeats an earlier variant".to_string();
            statuses.push(VariantStatus::rejected(&v.label, VariantState::Conflict, msg));
            continue;
        }
        let name = deployment_name(base, &v.label);
        if let Some(existing) = api.get_opt(&name).await? {
            let other = existing.owner_references().iter().find(|o| o.controller == Some(true) && &o.uid != uid);
            if let Some(other) = other {
                let msg = format!("deployment {name} is already managed by {} {}", other.kind, other.name);
                statuses.push(VariantStatus::rejected(&v.label, VariantState::Conflict, msg));
                continue;
            }
        }
        let deploy = build_deployment(base, Some(owner.ns.clone()), &owner.name, v, owner.controller_ref.clone());
        let applied = api.patch(&name, &apply, &Patch::Apply(&deploy)).await?;
        statuses.push(applied_status(&v.label, name.clone(), v.instances, Some(&applied)));
        wanted.insert(name);
    }

    // Prune. Checking the owner uid as well as the label means a Deployment
    // that merely carries the label, or belongs to a same-named owner of
    // the other kind, is never deleted.
    let selector = format!("{OWNER_LABEL}={}", owner.name);
    for existing in api.list(&ListParams::default().labels(&selector)).await? {
        let name = existing.name_any();
        let owned = existing.owner_references().iter().any(|o| &o.uid == uid);
        if owned && !wanted.contains(&name) {
            info!(%name, ns = %owner.ns, variant = ?existing.labels().get(MUTATION_LABEL), "pruning removed variant");
            api.delete(&name, &DeleteParams::background()).await?;
        }
    }
    Ok(statuses)
}

async fn reconcile_mutant(md: Arc<MutantDeployment>, ctx: Arc<Ctx>) -> Result<Action, MutantopsError> {
    let owner = Owner::of(md.as_ref())?;
    // A MutantDeployment has no status to report into, so problems are logged.
    for s in apply_variants(&ctx.client, &owner, &md.spec.deployment, &md.spec.mutations).await? {
        if let Some(msg) = s.message {
            warn!(mutant = %owner.name, ns = %owner.ns, variant = %s.label, state = ?s.state, "{msg}");
        }
    }
    Ok(Action::requeue(RESYNC))
}

async fn reconcile_mutation(m: Arc<Mutation>, ctx: Arc<Ctx>) -> Result<Action, MutantopsError> {
    if m.metadata.deletion_timestamp.is_some() {
        // Garbage collection removes its Deployments and Secret.
        return Ok(Action::await_change());
    }
    let owner = Owner::of(m.as_ref())?;
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &owner.ns);

    let mut status = MutationStatus::default();
    match ensure_secret(&secrets, &m).await? {
        Ok(name) => status.secret = Some(name),
        Err(msg) => status.message = Some(msg),
    }
    match validate_base(&m.spec.deployment) {
        // Nothing is applied or pruned from a broken base: what is running
        // keeps running until the base is fixed.
        Err(msg) => status.message = Some(msg),
        Ok(()) => status.variants = apply_variants(&ctx.client, &owner, &m.spec.deployment, &m.spec.variants).await?,
    }
    status.summary = Some(summarize(&status.variants));

    if m.status.as_ref() != Some(&status) {
        // Skipped when unchanged: every write is a watch event, which would
        // reconcile again.
        let api: Api<Mutation> = Api::namespaced(ctx.client.clone(), &owner.ns);
        let patch = json!({ "status": status });
        api.patch_status(&owner.name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }
    Ok(Action::requeue(RESYNC))
}

/// Creates the Mutation's Secret unless it exists, returning its name, or
/// why there is none as a message for the Mutation's status.
///
/// An existing Secret is never rewritten, so the token stays stable. One
/// that this Mutation does not own is left alone and reported: it was made
/// by someone else, and its contents are not ours to vouch for.
async fn ensure_secret(api: &Api<Secret>, m: &Mutation) -> Result<Result<String, String>, MutantopsError> {
    let name = secret_name(&m.name_any());
    let uid = m.uid();
    let owned = |s: &Secret| s.owner_references().iter().any(|o| Some(&o.uid) == uid.as_ref());

    if let Some(existing) = api.get_opt(&name).await? {
        return Ok(if owned(&existing) {
            Ok(name)
        } else {
            Err(format!("secret {name} exists but is not owned by this Mutation; not using it"))
        });
    }

    let owner = m
        .controller_owner_ref(&())
        .ok_or_else(|| MutantopsError::Invalid("Mutation has no uid".into()))?;
    let token = generate_token().map_err(|e| MutantopsError::Invalid(format!("no randomness for token: {e}")))?;
    match api.create(&PostParams::default(), &build_secret(m, owner, token)).await {
        Ok(_) => {
            info!(secret = %name, mutation = %m.name_any(), "created mutation secret");
            Ok(Ok(name))
        }
        // Lost a race with another reconcile; the next pass checks ownership.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(Ok(name)),
        // Invalid for this Mutation alone: report it there.
        Err(kube::Error::Api(e)) if e.code == 422 => {
            warn!(secret = %name, "cannot create mutation secret: {}", e.message);
            Ok(Err(format!("cannot create secret {name}: {}", e.message)))
        }
        Err(e) => Err(e.into()),
    }
}

fn error_policy<K: ResourceExt>(obj: Arc<K>, error: &MutantopsError, _ctx: Arc<Ctx>) -> Action {
    warn!(name = %obj.name_any(), ns = ?obj.namespace(), "reconcile failed: {error}");
    Action::requeue(Duration::from_secs(10))
}

/// Runs both controllers until a shutdown signal.
pub async fn run(client: Client) {
    let ctx = Arc::new(Ctx { client: client.clone() });
    // Each watches only the Deployments it generates: a hand edit or delete
    // is put back, and replica readiness flows into status.
    let owned = || watcher::Config::default().labels(OWNER_LABEL);

    let mutants = Controller::new(Api::<MutantDeployment>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Deployment>::all(client.clone()), owned())
        .shutdown_on_signal()
        .run(reconcile_mutant, error_policy, ctx.clone())
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(kind = "MutantDeployment", name = %obj.name, ns = ?obj.namespace, "reconciled"),
                Err(err) => warn!(kind = "MutantDeployment", "reconcile failed: {err}"),
            }
        });

    let mutations = Controller::new(Api::<Mutation>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Deployment>::all(client.clone()), owned())
        .shutdown_on_signal()
        .run(reconcile_mutation, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(kind = "Mutation", name = %obj.name, ns = ?obj.namespace, "reconciled"),
                Err(err) => warn!(kind = "Mutation", "reconcile failed: {err}"),
            }
        });

    futures::join!(mutants, mutations);
}
