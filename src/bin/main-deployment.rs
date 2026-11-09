use kube::Client;
use mutantops::{api, controller};
use std::net::SocketAddr;
use tracing::{info, warn};

/// Address the HTTP API listens on.
const DEFAULT_API_ADDR: &str = "0.0.0.0:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let client = Client::try_default().await?;

    // The API can create Deployments with arbitrary env in any namespace the
    // operator can write to, so it needs a token unless explicitly opted out.
    let token = std::env::var("MUTANT_API_TOKEN").ok().filter(|t| !t.is_empty());
    let insecure = std::env::var("MUTANT_API_INSECURE").is_ok_and(|v| v == "true");
    let api_enabled = token.is_some() || insecure;
    if token.is_none() && insecure {
        warn!("MUTANT_API_INSECURE=true: HTTP API is serving without authentication");
    }

    let addr: SocketAddr = std::env::var("MUTANT_API_ADDR")
        .unwrap_or_else(|_| DEFAULT_API_ADDR.into())
        .parse()?;

    let server = async {
        if !api_enabled {
            warn!("HTTP API disabled: set MUTANT_API_TOKEN (or MUTANT_API_INSECURE=true) to enable it");
            std::future::pending::<()>().await;
        }
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!(%addr, "HTTP API listening");
        axum::serve(listener, api::router(api::AppState::new(client.clone(), token)))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        anyhow::Ok(())
    };

    // The controller stops on SIGINT/SIGTERM; when it does, so does the API.
    tokio::select! {
        () = controller::run(client.clone()) => {}
        res = server => res?,
    }
    Ok(())
}
