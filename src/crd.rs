//! The `MutantDeployment` and `Mutation` resources, and the pure logic that
//! turns them into concrete `Deployment`s.
//!
//! A mutation can be declared two ways:
//! - inline, in `MutantDeployment.spec.mutations` (lives in git with the
//!   MutantDeployment, so GitOps tools own it);
//! - as a standalone `Mutation` resource naming its MutantDeployment in
//!   `spec.mutantDeployment` (created at runtime, e.g. through the HTTP API,
//!   without touching the GitOps-managed MutantDeployment).

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::EnvVar;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::{CustomResource, ResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Set on every generated Deployment: the mutation it was built from.
/// Kept unprefixed for compatibility with Deployments created before it
/// was namespaced.
pub const MUTATION_LABEL: &str = "mutation";
/// Set on every generated Deployment: the MutantDeployment that owns it.
/// Used to find Deployments to prune when a mutation is removed.
pub const OWNER_LABEL: &str = "mutantops.neiam.org/owner";
/// Set on Mutation resources the API creates: their MutantDeployment, so
/// they can be selected with `kubectl get mutations -l ...`.
pub const TARGET_LABEL: &str = "mutantops.neiam.org/mutant-deployment";

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(group = "neiam.org", version = "v1", kind = "MutantDeployment", namespaced)]
pub struct MutantDeploymentSpec {
    #[schemars(skip)]
    pub deployment: Deployment,
    #[serde(default)]
    pub mutations: Vec<MutationDef>,
}

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "neiam.org",
    version = "v1",
    kind = "Mutation",
    namespaced,
    status = "MutationStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct MutationSpec {
    /// Name of the MutantDeployment, in the same namespace, to mutate.
    pub mutant_deployment: String,
    #[serde(flatten)]
    pub def: MutationDef,
}

/// One mutation, however it was declared.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct MutationDef {
    pub label: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub instances: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MutationStatus {
    // `None` serializes as null on purpose: status is written as a merge
    // patch, where null clears a field left over from an earlier state.
    pub state: MutationState,
    #[serde(default)]
    pub message: Option<String>,
    /// The generated Deployment.
    #[serde(default)]
    pub deployment: Option<String>,
    #[serde(default)]
    pub replicas: Option<i32>,
    #[serde(default)]
    pub ready_replicas: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
pub enum MutationState {
    /// Deployment applied; not all replicas ready yet.
    #[default]
    Progressing,
    /// Every requested replica is ready.
    Ready,
    /// Not applied: the spec is invalid.
    Invalid,
    /// Not applied: another mutation of the same MutantDeployment already
    /// uses this label.
    Conflict,
}

impl MutationDef {
    /// Checks the mutation can become a valid Deployment, so a bad entry is
    /// refused at the API instead of failing reconciliation later.
    pub fn validate(&self) -> Result<(), String> {
        validate_label(&self.label)?;
        if self.instances < 0 {
            return Err(format!("instances must be >= 0, got {}", self.instances));
        }
        for key in self.env.keys() {
            validate_env_name(key)?;
        }
        Ok(())
    }
}

/// The label becomes both a name suffix and a label value, so it must be an
/// RFC 1123 label: lowercase alphanumerics and '-', 1-63 chars, alphanumeric
/// at both ends.
pub fn validate_label(label: &str) -> Result<(), String> {
    let ok = !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid label {label:?}: must be 1-63 lowercase alphanumerics or '-', \
             starting and ending alphanumeric"
        ))
    }
}

/// Kubernetes' rule for env var names: `[-._a-zA-Z][-._a-zA-Z0-9]*`.
fn validate_env_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    let first_ok = matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b"-._".contains(&b));
    if first_ok && bytes.all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)) {
        Ok(())
    } else {
        Err(format!("invalid env var name {name:?}"))
    }
}

impl MutantDeployment {
    /// Name of the base Deployment the mutations are derived from.
    pub fn base_name(&self) -> String {
        self.spec
            .deployment
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "mutant".into())
    }

    /// Name of the Deployment generated for `label`.
    pub fn deployment_name(&self, label: &str) -> String {
        format!("{}-{}", self.base_name(), label)
    }
}

/// Where an active mutation was declared.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Source {
    /// In the MutantDeployment's `spec.mutations`.
    Inline,
    /// A `Mutation` resource of this name.
    Resource { name: String },
}

/// The outcome of combining a MutantDeployment's inline mutations with the
/// Mutation resources that target it.
#[derive(Debug, Default)]
pub struct Resolved {
    /// Mutations to run, one per label.
    pub active: Vec<(MutationDef, Source)>,
    /// Mutation resources not run, by name, with why.
    pub rejected: Vec<(String, MutationState, String)>,
}

/// Decides which mutations run. Inline mutations come first, since they
/// live in git with the MutantDeployment; Mutation resources follow, oldest
/// first, so an existing mutation is never displaced by a newer one with the
/// same label. Invalid inline entries are dropped (the caller logs them).
pub fn resolve(mutant: &MutantDeployment, resources: &[Mutation]) -> Resolved {
    let mut out = Resolved::default();
    let mut labels = HashSet::new();

    for def in &mutant.spec.mutations {
        if def.validate().is_ok() && labels.insert(def.label.clone()) {
            out.active.push((def.clone(), Source::Inline));
        }
    }

    let mut targeting: Vec<_> = resources
        .iter()
        .filter(|m| m.spec.mutant_deployment == mutant.name_any() && m.metadata.deletion_timestamp.is_none())
        .collect();
    targeting.sort_by(|a, b| {
        (a.creation_timestamp(), a.name_any()).cmp(&(b.creation_timestamp(), b.name_any()))
    });
    for m in targeting {
        let def = &m.spec.def;
        if let Err(msg) = def.validate() {
            out.rejected.push((m.name_any(), MutationState::Invalid, msg));
        } else if !labels.insert(def.label.clone()) {
            out.rejected.push((
                m.name_any(),
                MutationState::Conflict,
                format!("label {:?} is already used by another mutation of {}", def.label, mutant.name_any()),
            ));
        } else {
            out.active.push((def.clone(), Source::Resource { name: m.name_any() }));
        }
    }
    out
}

/// Status of an applied mutation, from its Deployment's state.
pub fn applied_status(name: String, desired: i32, deploy: Option<&Deployment>) -> MutationStatus {
    let status = deploy.and_then(|d| d.status.as_ref());
    let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);
    let observed_current = deploy.is_some_and(|d| {
        d.status.as_ref().and_then(|s| s.observed_generation) >= d.metadata.generation
    });
    MutationStatus {
        state: if observed_current && ready >= desired {
            MutationState::Ready
        } else {
            MutationState::Progressing
        },
        message: None,
        deployment: Some(name),
        replicas: Some(desired),
        ready_replicas: Some(ready),
    }
}

/// Builds the Deployment for one mutation, ready for server-side apply.
pub fn build_deployment(
    mutant: &MutantDeployment,
    mutation: &MutationDef,
    owner: OwnerReference,
) -> Deployment {
    let base = &mutant.spec.deployment;
    let mut labels: BTreeMap<String, String> = base.metadata.labels.clone().unwrap_or_default();
    labels.insert(MUTATION_LABEL.into(), mutation.label.clone());
    labels.insert(OWNER_LABEL.into(), mutant.name_any());

    let mut deploy = Deployment {
        // Only the user-authored parts of the base metadata are carried over;
        // server-populated fields would make the apply fail.
        metadata: ObjectMeta {
            name: Some(mutant.deployment_name(&mutation.label)),
            namespace: mutant.namespace(),
            labels: Some(labels),
            annotations: base.metadata.annotations.clone(),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: base.spec.clone(),
        status: None,
    };

    if let Some(spec) = &mut deploy.spec {
        spec.replicas = Some(mutation.instances);
        // Pod labels (not the selector, which is immutable on existing
        // Deployments) so a Service or query can single out one mutation.
        spec.template
            .metadata
            .get_or_insert_with(Default::default)
            .labels
            .get_or_insert_with(Default::default)
            .insert(MUTATION_LABEL.into(), mutation.label.clone());

        if let Some(pod) = &mut spec.template.spec {
            let mut env: Vec<_> = mutation.env.iter().collect();
            env.sort(); // stable order, so re-applies don't churn the pod template
            for container in &mut pod.containers {
                let vars = container.env.get_or_insert_with(Vec::new);
                for (key, val) in &env {
                    // Override a variable the base already sets rather than
                    // adding a duplicate entry.
                    vars.retain(|v| &v.name != *key);
                    vars.push(EnvVar {
                        name: (*key).clone(),
                        value: Some((*val).clone()),
                        value_from: None,
                    });
                }
            }
        }
    }
    deploy
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    fn mutant(inline: Vec<MutationDef>) -> MutantDeployment {
        let deployment: Deployment = serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "nginx", "labels": { "app": "nginx" }, "resourceVersion": "9" },
            "spec": {
                "replicas": 3,
                "selector": { "matchLabels": { "app": "nginx" } },
                "template": {
                    "metadata": { "labels": { "app": "nginx" } },
                    "spec": { "containers": [{
                        "name": "nginx", "image": "nginx",
                        "env": [{ "name": "MODE", "value": "base" }]
                    }] }
                }
            }
        }))
        .unwrap();
        let mut md = MutantDeployment::new("nginx-mutant", MutantDeploymentSpec { deployment, mutations: inline });
        md.metadata.namespace = Some("default".into());
        md
    }

    fn def(label: &str) -> MutationDef {
        MutationDef {
            label: label.into(),
            env: HashMap::from([("MODE".into(), "canary".into()), ("EXTRA".into(), "1".into())]),
            instances: 1,
        }
    }

    fn resource(name: &str, target: &str, label: &str, created_secs: i64) -> Mutation {
        let mut m = Mutation::new(name, MutationSpec { mutant_deployment: target.into(), def: def(label) });
        m.metadata.creation_timestamp = Some(Time(k8s_openapi::chrono::DateTime::from_timestamp(created_secs, 0).unwrap()));
        m
    }

    #[test]
    fn builds_deployment() {
        let md = mutant(vec![]);
        let d = build_deployment(&md, &def("canary"), OwnerReference::default());
        assert_eq!(d.metadata.name.as_deref(), Some("nginx-canary"));
        assert_eq!(d.metadata.namespace.as_deref(), Some("default"));
        assert_eq!(d.metadata.resource_version, None);
        let labels = d.metadata.labels.unwrap();
        assert_eq!(labels[MUTATION_LABEL], "canary");
        assert_eq!(labels[OWNER_LABEL], "nginx-mutant");
        assert_eq!(labels["app"], "nginx");

        let spec = d.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.template.metadata.unwrap().labels.unwrap()[MUTATION_LABEL], "canary");
        let env = spec.template.spec.unwrap().containers[0].env.clone().unwrap();
        let names: Vec<_> = env.iter().map(|e| (e.name.as_str(), e.value.as_deref().unwrap())).collect();
        assert_eq!(names, [("EXTRA", "1"), ("MODE", "canary")]);
    }

    #[test]
    fn validates() {
        assert!(def("canary").validate().is_ok());
        for bad in ["", "Canary", "-a", "a-", "a_b", &"a".repeat(64)] {
            assert!(validate_label(bad).is_err(), "{bad:?}");
        }
        let mut m = def("ok");
        m.instances = -1;
        assert!(m.validate().is_err());
        let mut m = def("ok");
        m.env.insert("1BAD".into(), "x".into());
        assert!(m.validate().is_err());
    }

    #[test]
    fn resolves_inline_first_then_oldest_resource() {
        let md = mutant(vec![def("canary")]);
        let mut bad = resource("bad", "nginx-mutant", "Nope", 1);
        bad.spec.def.label = "Nope".into();
        let resources = [
            resource("newer-beta", "nginx-mutant", "beta", 20),
            resource("older-beta", "nginx-mutant", "beta", 10),
            resource("dup-canary", "nginx-mutant", "canary", 5),
            resource("elsewhere", "other-mutant", "gamma", 1),
            bad,
        ];
        let r = resolve(&md, &resources);

        let active: Vec<_> = r.active.iter().map(|(d, s)| (d.label.as_str(), s.clone())).collect();
        assert_eq!(
            active,
            [
                ("canary", Source::Inline),
                ("beta", Source::Resource { name: "older-beta".into() }),
            ]
        );
        let rejected: Vec<_> = r.rejected.iter().map(|(n, s, _)| (n.as_str(), *s)).collect();
        assert_eq!(
            rejected,
            [
                ("bad", MutationState::Invalid),
                ("dup-canary", MutationState::Conflict),
                ("newer-beta", MutationState::Conflict),
            ]
        );
    }

    #[test]
    fn status_from_deployment() {
        let mut d: Deployment = serde_json::from_value(serde_json::json!({
            "metadata": { "generation": 2 },
            "status": { "observedGeneration": 2, "readyReplicas": 2 }
        }))
        .unwrap();
        assert_eq!(applied_status("x".into(), 2, Some(&d)).state, MutationState::Ready);
        assert_eq!(applied_status("x".into(), 3, Some(&d)).state, MutationState::Progressing);
        d.metadata.generation = Some(3); // spec changed, not yet observed
        assert_eq!(applied_status("x".into(), 2, Some(&d)).state, MutationState::Progressing);
        assert_eq!(applied_status("x".into(), 0, None).state, MutationState::Progressing);
    }

    #[test]
    fn mutation_wire_format() {
        let m: MutationSpec = serde_json::from_value(serde_json::json!({
            "mutantDeployment": "nginx-mutant", "label": "beta", "env": { "A": "1" }, "instances": 2
        }))
        .unwrap();
        assert_eq!(m.mutant_deployment, "nginx-mutant");
        assert_eq!(m.def.label, "beta");
    }
}
