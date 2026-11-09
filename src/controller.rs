//! Reconciles each MutantDeployment, together with the Mutation resources
//! that target it, into one Deployment per mutation; prunes Deployments
//! whose mutation is gone; and reports each Mutation's state in its status.

use crate::crd::{
    applied_status, build_deployment, resolve, MutantDeployment, Mutation, MutationStatus, Source,
    MUTATION_LABEL, OWNER_LABEL,
};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use kube::{
    api::{Api, DeleteParams, ListParams, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        reflector::ObjectRef,
        watcher,
    },
    Client, Resource, ResourceExt,
};
use serde_json::json;
use std::{collections::HashSet, fmt, sync::Arc, time::Duration};
use tracing::{info, warn};

const FIELD_MANAGER: &str = "mutantops-operator";
/// Periodic resync. Events drive reconciliation; this only catches what no
/// event points at, such as a Mutation retargeted away from a
/// MutantDeployment, whose old Deployment is then pruned.
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

async fn reconcile(mutant: Arc<MutantDeployment>, ctx: Arc<Ctx>) -> Result<Action, MutantopsError> {
    let ns = mutant
        .namespace()
        .ok_or_else(|| MutantopsError::Invalid("MutantDeployment has no namespace".into()))?;
    let controller_ref = mutant
        .controller_owner_ref(&())
        .ok_or_else(|| MutantopsError::Invalid("MutantDeployment has no uid".into()))?;
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let mutations: Api<Mutation> = Api::namespaced(ctx.client.clone(), &ns);
    let apply = PatchParams::apply(FIELD_MANAGER).force();

    // Inline entries can only be edited in the MutantDeployment itself, and
    // have no status to report into, so a bad one is just logged.
    for def in &mutant.spec.mutations {
        if let Err(msg) = def.validate() {
            warn!(mutant = %mutant.name_any(), %ns, "skipping inline mutation: {msg}");
        }
    }

    let resources = mutations.list(&ListParams::default()).await?.items;
    // Each Mutation becomes a dependent of its MutantDeployment: Argo CD then
    // shows it in the MutantDeployment's resource tree, and deleting the
    // MutantDeployment garbage-collects it. Done before the status writes
    // below, which would invalidate the resourceVersion this is guarded by.
    let owner = mutant.owner_ref(&()).expect("uid checked above");
    for m in resources.iter().filter(|m| m.spec.mutant_deployment == mutant.name_any()) {
        if !m.owner_references().iter().any(|o| o.uid == owner.uid) {
            let mut refs = m.owner_references().to_vec();
            refs.push(owner.clone());
            let patch = json!({ "metadata": { "ownerReferences": refs, "resourceVersion": m.resource_version() } });
            mutations.patch(&m.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
        }
    }

    let resolved = resolve(&mutant, &resources);
    let find = |name: &str| resources.iter().find(|m| m.name_any() == name);

    let mut wanted = HashSet::new();
    for (def, source) in &resolved.active {
        let name = mutant.deployment_name(&def.label);
        let deploy = build_deployment(&mutant, def, controller_ref.clone());
        let applied = deployments.patch(&name, &apply, &Patch::Apply(&deploy)).await?;
        if let Source::Resource { name: mname } = source {
            if let Some(m) = find(mname) {
                set_status(&mutations, m, applied_status(name.clone(), def.instances, Some(&applied))).await?;
            }
        }
        wanted.insert(name);
    }
    for (mname, state, message) in &resolved.rejected {
        if let Some(m) = find(mname) {
            let status = MutationStatus { state: *state, message: Some(message.clone()), ..Default::default() };
            set_status(&mutations, m, status).await?;
        }
    }

    // Prune: Deployments this MutantDeployment owns whose mutation is gone.
    // Checking the owner uid as well as the label means a Deployment that
    // merely carries the label is never deleted.
    let selector = format!("{OWNER_LABEL}={}", mutant.name_any());
    for existing in deployments.list(&ListParams::default().labels(&selector)).await? {
        let name = existing.name_any();
        let owned = existing.owner_references().iter().any(|o| o.uid == controller_ref.uid);
        if owned && !wanted.contains(&name) {
            info!(%name, %ns, mutation = ?existing.labels().get(MUTATION_LABEL), "pruning removed mutation");
            deployments.delete(&name, &DeleteParams::background()).await?;
        }
    }

    Ok(Action::requeue(RESYNC))
}

/// Writes `status` unless it is already current. Skipping no-op writes
/// matters: every write is a watch event that reconciles again.
async fn set_status(api: &Api<Mutation>, m: &Mutation, status: MutationStatus) -> Result<(), kube::Error> {
    if m.status.as_ref() == Some(&status) {
        return Ok(());
    }
    let patch = json!({ "status": status });
    api.patch_status(&m.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

fn error_policy(mutant: Arc<MutantDeployment>, error: &MutantopsError, _ctx: Arc<Ctx>) -> Action {
    warn!(mutant = %mutant.name_any(), "reconcile failed: {error}");
    Action::requeue(Duration::from_secs(10))
}

/// Runs the controller until a shutdown signal.
pub async fn run(client: Client) {
    let mutants = Api::<MutantDeployment>::all(client.clone());
    Controller::new(mutants, watcher::Config::default())
        // Watch only the Deployments we generate: a hand edit or delete is
        // put back, and replica readiness flows into Mutation status.
        .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default().labels(OWNER_LABEL))
        // A Mutation created, changed or deleted reconciles its target.
        .watches(Api::<Mutation>::all(client.clone()), watcher::Config::default(), |m| {
            let ns = m.namespace()?;
            Some(ObjectRef::new(&m.spec.mutant_deployment).within(&ns))
        })
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::new(Ctx { client }))
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(name = %obj.name, ns = ?obj.namespace, "reconciled"),
                Err(err) => warn!("reconcile failed: {err}"),
            }
        })
        .await;
}
