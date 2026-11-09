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

    // Requests about one Mutation are authorized by that Mutation's own
    // generated token. The admin token, optional, covers the rest: listing,
    // creating and deleting Mutations.
    let admin_token = std::env::var("MUTANT_API_TOKEN").ok().filter(|t| !t.is_empty());
    let insecure = std::env::var("MUTANT_API_INSECURE").is_ok_and(|v| v == "true");
    if insecure {
        warn!("MUTANT_API_INSECURE=true: HTTP API is serving without authentication");
    } else if admin_token.is_none() {
        info!("no MUTANT_API_TOKEN: only Mutation tokens are accepted");
    }

    let addr: SocketAddr = std::env::var("MUTANT_API_ADDR")
        .unwrap_or_else(|_| DEFAULT_API_ADDR.into())
        .parse()?;

    let server = async {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!(%addr, "HTTP API listening");
        axum::serve(listener, api::router(api::AppState::new(client.clone(), admin_token, insecure)))
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
