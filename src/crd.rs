//! The `MutantDeployment` and `Mutation` resources, and the pure logic that
//! turns them into concrete `Deployment`s.
//!
//! Both are a base Deployment plus variants of it, one Deployment per
//! variant. They differ in who configures them:
//! - a `MutantDeployment` is declared in git (its variants are
//!   `spec.mutations`), so GitOps tools own it;
//! - a `Mutation` is configured at runtime through the operator's HTTP API
//!   (its variants are `spec.variants`), and gets a generated Secret.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{EnvVar, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::ByteString;
use kube::{CustomResource, ResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Set on every generated Deployment: the variant it was built from.
/// Kept unprefixed for compatibility with Deployments created before it
/// was namespaced.
pub const MUTATION_LABEL: &str = "mutation";
/// Set on every generated Deployment: the name of the MutantDeployment or
/// Mutation that owns it. Used to find Deployments to prune.
pub const OWNER_LABEL: &str = "mutantops.neiam.org/owner";
/// Set on each Mutation's generated Secret: the Mutation it belongs to.
pub const SECRET_MUTATION_LABEL: &str = "mutantops.neiam.org/mutation";
/// Key of the generated token in a Mutation's Secret.
pub const SECRET_TOKEN_KEY: &str = "token";

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(group = "neiam.org", version = "v1", kind = "MutantDeployment", namespaced)]
pub struct MutantDeploymentSpec {
    #[schemars(skip)]
    pub deployment: Deployment,
    #[serde(default)]
    pub mutations: Vec<Variant>,
}

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "neiam.org",
    version = "v1",
    kind = "Mutation",
    namespaced,
    status = "MutationStatus"
)]
pub struct MutationSpec {
    /// The base Deployment every variant is derived from.
    #[schemars(skip)]
    pub deployment: Deployment,
    #[serde(default)]
    pub variants: Vec<Variant>,
}

/// One variant of a base Deployment: its own name suffix, env overrides
/// and replica count.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Variant {
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
    /// e.g. "2/3 variants ready", for `kubectl get mutations`.
    #[serde(default)]
    pub summary: Option<String>,
    /// The Mutation's generated Secret, holding a random `token`.
    #[serde(default)]
    pub secret: Option<String>,
    /// A problem with the Mutation as a whole, such as its Secret.
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub variants: Vec<VariantStatus>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VariantStatus {
    pub label: String,
    pub state: VariantState,
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
pub enum VariantState {
    /// Deployment applied; not all replicas ready yet.
    #[default]
    Progressing,
    /// Every requested replica is ready.
    Ready,
    /// Not applied: the variant is invalid.
    Invalid,
    /// Not applied: its label repeats an earlier variant's, or its
    /// Deployment name is already taken by another MutantDeployment or
    /// Mutation.
    Conflict,
}

impl VariantStatus {
    pub fn rejected(label: &str, state: VariantState, message: String) -> Self {
        Self { label: label.into(), state, message: Some(message), deployment: None, replicas: None, ready_replicas: None }
    }
}

impl Variant {
    /// Checks the variant can become a valid Deployment, so a bad entry is
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

/// Checks every variant, and that no two share a label.
pub fn validate_variants(variants: &[Variant]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for v in variants {
        v.validate()?;
        if !seen.insert(&v.label) {
            return Err(format!("variant label {:?} appears more than once", v.label));
        }
    }
    Ok(())
}

/// Checks a base Deployment can have variants built from it.
pub fn validate_base(base: &Deployment) -> Result<(), String> {
    match base.metadata.name.as_deref() {
        None | Some("") => return Err("deployment.metadata.name is required: variants are named after it".into()),
        Some(_) => {}
    }
    if base.spec.as_ref().and_then(|s| s.template.spec.as_ref()).is_none() {
        return Err("deployment.spec.template.spec is required".into());
    }
    Ok(())
}

/// Labels become both a name suffix and a label value, and Mutation names
/// a Secret name prefix, so both must be RFC 1123 labels: lowercase
/// alphanumerics and '-', 1-63 chars, alphanumeric at both ends.
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
            "invalid name {label:?}: must be 1-63 lowercase alphanumerics or '-', \
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

/// Name of the base Deployment variants are derived from.
pub fn base_name(base: &Deployment) -> String {
    base.metadata.name.clone().unwrap_or_else(|| "mutant".into())
}

/// Name of the Deployment generated for variant `label` of `base`.
pub fn deployment_name(base: &Deployment, label: &str) -> String {
    format!("{}-{}", base_name(base), label)
}

/// Adds `variant` to `list`, refusing a duplicate label.
pub fn insert_variant(list: &mut Vec<Variant>, variant: Variant) -> Result<(), String> {
    if list.iter().any(|v| v.label == variant.label) {
        return Err(format!("variant {:?} already exists", variant.label));
    }
    list.push(variant);
    Ok(())
}

/// Removes the variant labelled `label`, returning it if it was present.
pub fn remove_variant(list: &mut Vec<Variant>, label: &str) -> Option<Variant> {
    let idx = list.iter().position(|v| v.label == label)?;
    Some(list.remove(idx))
}

/// Status of an applied variant, from its Deployment's state.
pub fn applied_status(label: &str, name: String, desired: i32, deploy: Option<&Deployment>) -> VariantStatus {
    let ready = deploy.and_then(|d| d.status.as_ref()).and_then(|s| s.ready_replicas).unwrap_or(0);
    let observed_current = deploy.is_some_and(|d| {
        d.status.as_ref().and_then(|s| s.observed_generation) >= d.metadata.generation
    });
    VariantStatus {
        label: label.into(),
        state: if observed_current && ready >= desired { VariantState::Ready } else { VariantState::Progressing },
        message: None,
        deployment: Some(name),
        replicas: Some(desired),
        ready_replicas: Some(ready),
    }
}

/// e.g. "2/3 variants ready".
pub fn summarize(variants: &[VariantStatus]) -> String {
    let ready = variants.iter().filter(|v| v.state == VariantState::Ready).count();
    format!("{ready}/{} variants ready", variants.len())
}

/// Markers Argo CD puts on what it syncs, by tracking method: the
/// annotation (Argo CD 3's default) and the label (earlier default).
const ARGO_TRACKING_ANNOTATION: &str = "argocd.argoproj.io/tracking-id";
const ARGO_TRACKING_LABEL: &str = "app.kubernetes.io/instance";

/// Opts a GitOps-managed Mutation's variants back into the API: git holds
/// the base, the API the variants. Its manifest must then leave `variants`
/// out, so the sync never touches them.
pub const RUNTIME_VARIANTS_ANNOTATION: &str = "mutantops.neiam.org/runtime-variants";

/// Whether a GitOps tool owns this Mutation, so it changes in git and the
/// API must not delete it (the sync would only put it back).
pub fn is_gitops_managed(m: &Mutation) -> bool {
    m.annotations().contains_key(ARGO_TRACKING_ANNOTATION) || m.labels().contains_key(ARGO_TRACKING_LABEL)
}

/// Whether the API may add and remove this Mutation's variants: any Mutation
/// made at runtime, and a GitOps-managed one that opts in with
/// `mutantops.neiam.org/runtime-variants: "true"`.
pub fn variants_editable(m: &Mutation) -> bool {
    !is_gitops_managed(m) || m.annotations().get(RUNTIME_VARIANTS_ANNOTATION).is_some_and(|v| v == "true")
}

/// Name of the Secret generated for the Mutation named `mutation`.
pub fn secret_name(mutation: &str) -> String {
    format!("{mutation}-secret")
}

/// The Secret generated for `mutation`: a random `token`, owned by the
/// Mutation so it is garbage-collected with it. Created once and never
/// rewritten, so the token is stable for the Mutation's lifetime.
pub fn build_secret(mutation: &Mutation, owner: OwnerReference, token: String) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(secret_name(&mutation.name_any())),
            namespace: mutation.namespace(),
            labels: Some(BTreeMap::from([(SECRET_MUTATION_LABEL.to_string(), mutation.name_any())])),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        type_: Some("Opaque".into()),
        data: Some(BTreeMap::from([(SECRET_TOKEN_KEY.to_string(), ByteString(token.into_bytes()))])),
        ..Default::default()
    }
}

/// 32 random bytes from the OS, hex-encoded.
pub fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Builds the Deployment for one variant of `base`, ready for server-side
/// apply, owned by `owner` (named `owner_name`) in namespace `ns`.
pub fn build_deployment(
    base: &Deployment,
    ns: Option<String>,
    owner_name: &str,
    variant: &Variant,
    owner: OwnerReference,
) -> Deployment {
    let mut labels: BTreeMap<String, String> = base.metadata.labels.clone().unwrap_or_default();
    labels.insert(MUTATION_LABEL.into(), variant.label.clone());
    labels.insert(OWNER_LABEL.into(), owner_name.into());

    let mut deploy = Deployment {
        // Only the user-authored parts of the base metadata are carried over;
        // server-populated fields would make the apply fail.
        metadata: ObjectMeta {
            name: Some(deployment_name(base, &variant.label)),
            namespace: ns,
            labels: Some(labels),
            annotations: base.metadata.annotations.clone(),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: base.spec.clone(),
        status: None,
    };

    if let Some(spec) = &mut deploy.spec {
        spec.replicas = Some(variant.instances);
        // Pod labels (not the selector, which is immutable on existing
        // Deployments) so a Service or query can single out one variant.
        spec.template
            .metadata
            .get_or_insert_with(Default::default)
            .labels
            .get_or_insert_with(Default::default)
            .insert(MUTATION_LABEL.into(), variant.label.clone());

        if let Some(pod) = &mut spec.template.spec {
            let mut env: Vec<_> = variant.env.iter().collect();
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

    fn base() -> Deployment {
        serde_json::from_value(serde_json::json!({
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
        .unwrap()
    }

    fn variant(label: &str) -> Variant {
        Variant {
            label: label.into(),
            env: HashMap::from([("MODE".into(), "canary".into()), ("EXTRA".into(), "1".into())]),
            instances: 1,
        }
    }

    fn mutation(name: &str) -> Mutation {
        let mut m = Mutation::new(name, MutationSpec { deployment: base(), variants: vec![variant("a")] });
        m.metadata.namespace = Some("default".into());
        m
    }

    #[test]
    fn builds_deployment() {
        let d = build_deployment(&base(), Some("default".into()), "web", &variant("canary"), OwnerReference::default());
        assert_eq!(d.metadata.name.as_deref(), Some("nginx-canary"));
        assert_eq!(d.metadata.namespace.as_deref(), Some("default"));
        assert_eq!(d.metadata.resource_version, None);
        let labels = d.metadata.labels.unwrap();
        assert_eq!(labels[MUTATION_LABEL], "canary");
        assert_eq!(labels[OWNER_LABEL], "web");
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
        assert!(variant("canary").validate().is_ok());
        for bad in ["", "Canary", "-a", "a-", "a_b", &"a".repeat(64)] {
            assert!(validate_label(bad).is_err(), "{bad:?}");
        }
        let mut v = variant("ok");
        v.instances = -1;
        assert!(v.validate().is_err());
        let mut v = variant("ok");
        v.env.insert("1BAD".into(), "x".into());
        assert!(v.validate().is_err());

        assert!(validate_variants(&[variant("a"), variant("b")]).is_ok());
        assert!(validate_variants(&[variant("a"), variant("a")]).is_err());

        assert!(validate_base(&base()).is_ok());
        let mut nameless = base();
        nameless.metadata.name = None;
        assert!(validate_base(&nameless).is_err());
        assert!(validate_base(&Deployment::default()).is_err());
    }

    #[test]
    fn inserts_and_removes() {
        let mut list = vec![variant("a")];
        assert!(insert_variant(&mut list, variant("a")).is_err());
        insert_variant(&mut list, variant("b")).unwrap();
        assert_eq!(remove_variant(&mut list, "a").map(|v| v.label), Some("a".into()));
        assert_eq!(remove_variant(&mut list, "a"), None);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn status_from_deployment() {
        let mut d: Deployment = serde_json::from_value(serde_json::json!({
            "metadata": { "generation": 2 },
            "status": { "observedGeneration": 2, "readyReplicas": 2 }
        }))
        .unwrap();
        assert_eq!(applied_status("x", "d".into(), 2, Some(&d)).state, VariantState::Ready);
        assert_eq!(applied_status("x", "d".into(), 3, Some(&d)).state, VariantState::Progressing);
        d.metadata.generation = Some(3); // spec changed, not yet observed
        assert_eq!(applied_status("x", "d".into(), 2, Some(&d)).state, VariantState::Progressing);
        assert_eq!(applied_status("x", "d".into(), 0, None).state, VariantState::Progressing);

        let ready = applied_status("a", "d".into(), 0, Some(&d));
        let rejected = VariantStatus::rejected("b", VariantState::Conflict, "taken".into());
        assert_eq!(summarize(&[ready, rejected]), "0/2 variants ready");
    }

    #[test]
    fn detects_gitops_managed() {
        let mut m = mutation("a");
        assert!(!is_gitops_managed(&m));
        m.annotations_mut().insert(ARGO_TRACKING_ANNOTATION.into(), "crib:neiam.org/Mutation:x/a".into());
        assert!(is_gitops_managed(&m));
        let mut m = mutation("b");
        m.labels_mut().insert(ARGO_TRACKING_LABEL.into(), "crib".into());
        assert!(is_gitops_managed(&m));

        // Runtime-made: editable. Argo-synced: not, unless it opts in.
        assert!(variants_editable(&mutation("c")));
        assert!(!variants_editable(&m));
        m.annotations_mut().insert(RUNTIME_VARIANTS_ANNOTATION.into(), "true".into());
        assert!(variants_editable(&m));
        assert!(is_gitops_managed(&m), "still not deletable through the API");
    }

    #[test]
    fn builds_secret() {
        let m = mutation("browser-snap");
        let s = build_secret(&m, OwnerReference { uid: "u1".into(), ..Default::default() }, "abc".into());
        assert_eq!(s.metadata.name.as_deref(), Some("browser-snap-secret"));
        assert_eq!(s.metadata.namespace.as_deref(), Some("default"));
        assert_eq!(s.metadata.owner_references.unwrap()[0].uid, "u1");
        assert_eq!(s.metadata.labels.unwrap()[SECRET_MUTATION_LABEL], "browser-snap");
        assert_eq!(s.data.unwrap()[SECRET_TOKEN_KEY].0, b"abc");
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (generate_token().unwrap(), generate_token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn wire_format() {
        let m: MutationSpec = serde_json::from_value(serde_json::json!({
            "deployment": { "metadata": { "name": "browser-snap" } },
            "variants": [{ "label": "lilygo47", "env": { "SNAP_RES": "960x540" }, "instances": 1 }]
        }))
        .unwrap();
        assert_eq!(base_name(&m.deployment), "browser-snap");
        assert_eq!(m.variants[0].label, "lilygo47");
        // A MutantDeployment's variants keep their `mutations` key.
        let md: MutantDeploymentSpec = serde_json::from_value(serde_json::json!({
            "deployment": {}, "mutations": [{ "label": "a", "instances": 1 }]
        }))
        .unwrap();
        assert_eq!(md.mutations[0].label, "a");
    }
}
