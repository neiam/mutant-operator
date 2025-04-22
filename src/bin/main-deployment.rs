use futures::StreamExt;
use k8s_openapi::api::{apps::v1::Deployment, core::v1::EnvVar};
use kube::{
    api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams},
    runtime::controller::{self, Action, Controller},
    Client, CustomResource, ResourceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, sync::Arc};
use kube::api::{Patch, PatchParams};
use kube::runtime::watcher;
use tokio::time::Duration;
use tracing::{info, instrument};
use std::error::Error;

// Define a custom error type
#[derive(Debug)]
pub enum MutantopsError {
    ParseError(String),
    IoError(std::io::Error),
    KubeError(kube::Error),
}

impl std::error::Error for MutantopsError {}

// Implement the `fmt::Display` trait for `MyError`
impl fmt::Display for MutantopsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MutantopsError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            MutantopsError::IoError(err) => write!(f, "IO error: {}", err),
            MutantopsError::KubeError(err) => write!(f, "Kube error: {}", err),
        }
    }
}

// Implement the `From` trait for `std::io::Error`
impl From<std::io::Error> for MutantopsError {
    fn from(err: std::io::Error) -> Self {
        MutantopsError::IoError(err)
    }
}

// Implement the `From` trait for `kube::Error`
impl From<kube::Error> for MutantopsError {
    fn from(err: kube::Error) -> Self {
        MutantopsError::KubeError(err)
    }
}

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(group = "neiam.org", version = "v1", kind = "MutantDeployment", namespaced)]
pub struct MutantDeploymentSpec {
    #[schemars(skip)]
    pub deployment: Deployment,
    pub mutations: Vec<Mutation>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct Mutation {
    pub label: String,
    pub env: HashMap<String, String>,
    pub instances: i32,
}

// #[instrument(skip(_ctx))]
async fn reconcile(mutant: Arc<MutantDeployment>, ctx: Arc<()>) -> Result<Action, MutantopsError>  {
    let client = Client::try_default().await?;
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &mutant.namespace().unwrap());

    let base_deploy = &mutant.spec.deployment;

    for mutation in &mutant.spec.mutations {
        let mut new_deploy = base_deploy.clone();

        // Modify name
        let name = format!("{}-{}", base_deploy.metadata.name.clone().unwrap_or("mutant".into()), mutation.label);
        new_deploy.metadata.name = Some(name.clone());
        new_deploy.metadata.labels.get_or_insert_with(Default::default).insert("mutation".to_string(), mutation.label.clone());

        // Modify replicas
        if let Some(spec) = &mut new_deploy.spec {
            spec.replicas = Some(mutation.instances);

            if let Some(template) = &mut spec.template.spec {
                for container in &mut template.containers {
                    let envs = container.env.get_or_insert(vec![]);
                    for (key, val) in &mutation.env {
                        envs.push(k8s_openapi::api::core::v1::EnvVar {
                            name: key.clone(),
                            value: Some(val.clone()),
                            value_from: None,
                        });
                    }
                }
            }
        }

        // Apply the new deployment
        match deployments.get(&name).await {
            Ok(_) => {
                deployments.replace(&name, &PostParams::default(), &new_deploy).await?;
            }
            Err(_) => {
                deployments.create(&PostParams::default(), &new_deploy).await?;
            }
        }
    }

    Ok(Action::await_change())
}

fn error_policy(_object: Arc<MutantDeployment>, error: &MutantopsError, _ctx: Arc<()>) -> controller::Action {
    println!("Error reconciling: {:?}", error);
    controller::Action::requeue(Duration::from_secs(10))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let client = Client::try_default().await?;
    let crds = Api::<MutantDeployment>::all(client.clone());

    Controller::new(crds, watcher::Config::default())
        .run(reconcile, error_policy, Arc::new(()))
        .for_each(|res| async move {
            match res {
                Ok((mutant, _)) => info!("Reconciled {:?}", mutant.name),
                Err(err) => info!("Reconcile failed: {:?}", err),
            }
        })
        .await;

    Ok(())
}
