//! Prometheus metrics, on their own listener (`MUTANT_METRICS_ADDR`, default
//! `0.0.0.0:9568`).
//!
//! Not on the HTTP API's port: that one is behind a Service other namespaces
//! reach, and `/metrics` there would be one more thing to authorize. The
//! cluster's Prometheus finds this listener from the pod's `prometheus.io/*`
//! annotations.
//!
//! Every name is `mutant_operator_*`, except the process collector's standard
//! `process_*` series. `kind` is the custom resource's kind, `MutantDeployment`
//! or `Mutation`.
//!
//! - **Reconciles** (`mutant_operator_reconciliations_total`,
//!   `mutant_operator_reconcile_duration_seconds`,
//!   `mutant_operator_reconcile_errors_total`) are counted as they happen and
//!   reset when the pod restarts: use `rate`/`increase`.
//! - **What is managed** (`mutant_operator_managed_objects`,
//!   `mutant_operator_variants`, `mutant_operator_mutation_variants`) is read
//!   from the controllers' caches at scrape time, so a deleted object drops
//!   out at once rather than waiting for a reconcile.

use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use kube::runtime::reflector::Store;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};
use tracing::error;

use crate::crd::{MutantDeployment, Mutation, VariantState};

/// Default listen address for `/metrics`: the fleet's port for a separate
/// metrics listener.
pub const DEFAULT_METRICS_ADDR: &str = "0.0.0.0:9568";

pub const KIND_MUTANT: &str = "MutantDeployment";
pub const KIND_MUTATION: &str = "Mutation";

/// A reconcile runs a handful of API calls: tens of milliseconds normally,
/// seconds when the API server is slow.
const DURATION_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

const VARIANT_STATES: [VariantState; 4] =
    [VariantState::Progressing, VariantState::Ready, VariantState::Invalid, VariantState::Conflict];

struct Metrics {
    registry: Registry,
    reconciliations: IntCounterVec,
    reconcile_duration: HistogramVec,
    reconcile_errors: IntCounterVec,
    controller_errors: IntCounterVec,
    managed_objects: IntGaugeVec,
    variants: IntGaugeVec,
    mutation_variants: IntGaugeVec,
}

fn register<C: prometheus::core::Collector + Clone + 'static>(registry: &Registry, c: C) -> C {
    registry.register(Box::new(c.clone())).expect("unique metric");
    c
}

static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let r = Registry::new();

    #[cfg(target_os = "linux")]
    r.register(Box::new(prometheus::process_collector::ProcessCollector::for_self()))
        .expect("process collector");

    let build_info = register(
        &r,
        IntGauge::with_opts(
            Opts::new("mutant_operator_build_info", "Always 1; version is the crate version running.")
                .const_label("version", env!("CARGO_PKG_VERSION")),
        )
        .expect("valid gauge"),
    );
    build_info.set(1);

    let m = Metrics {
        reconciliations: register(
            &r,
            IntCounterVec::new(
                Opts::new(
                    "mutant_operator_reconciliations_total",
                    "Reconciles finished. outcome: ok or error.",
                ),
                &["kind", "outcome"],
            )
            .expect("valid counter"),
        ),
        reconcile_duration: register(
            &r,
            HistogramVec::new(
                HistogramOpts::new(
                    "mutant_operator_reconcile_duration_seconds",
                    "Time one reconcile took. outcome: ok or error.",
                )
                .buckets(DURATION_BUCKETS.to_vec()),
                &["kind", "outcome"],
            )
            .expect("valid histogram"),
        ),
        reconcile_errors: register(
            &r,
            IntCounterVec::new(
                Opts::new(
                    "mutant_operator_reconcile_errors_total",
                    "Failed reconciles, retried after 10s. error: invalid (the resource itself), \
                     api (the API server refused a request) or client (it was never answered).",
                ),
                &["kind", "error"],
            )
            .expect("valid counter"),
        ),
        controller_errors: register(
            &r,
            IntCounterVec::new(
                Opts::new(
                    "mutant_operator_controller_errors_total",
                    "Controller errors other than a failed reconcile. error: queue (a watch failed, \
                     so changes are not being seen), object_not_found or runner.",
                ),
                &["kind", "error"],
            )
            .expect("valid counter"),
        ),
        managed_objects: register(
            &r,
            IntGaugeVec::new(
                Opts::new(
                    "mutant_operator_managed_objects",
                    "Custom resources in the controller's cache, cluster-wide.",
                ),
                &["kind"],
            )
            .expect("valid gauge"),
        ),
        variants: register(
            &r,
            IntGaugeVec::new(
                Opts::new(
                    "mutant_operator_variants",
                    "Variants declared in specs, summed over every resource of the kind: \
                     the Deployments the operator is asked to run.",
                ),
                &["kind"],
            )
            .expect("valid gauge"),
        ),
        mutation_variants: register(
            &r,
            IntGaugeVec::new(
                Opts::new(
                    "mutant_operator_mutation_variants",
                    "Mutation variants by the state their Mutation's status reports: \
                     Progressing, Ready, Invalid or Conflict.",
                ),
                &["state"],
            )
            .expect("valid gauge"),
        ),
        registry: r,
    };

    // Present from the first scrape, so a panel shows 0 rather than nothing.
    for kind in [KIND_MUTANT, KIND_MUTATION] {
        m.managed_objects.with_label_values(&[kind]).set(0);
        m.variants.with_label_values(&[kind]).set(0);
        for outcome in ["ok", "error"] {
            m.reconciliations.with_label_values(&[kind, outcome]);
        }
    }
    for state in VARIANT_STATES {
        m.mutation_variants.with_label_values(&[state_label(state)]).set(0);
    }
    m
});

fn state_label(state: VariantState) -> &'static str {
    match state {
        VariantState::Progressing => "Progressing",
        VariantState::Ready => "Ready",
        VariantState::Invalid => "Invalid",
        VariantState::Conflict => "Conflict",
    }
}

/// The controllers' caches, read at scrape time.
static STORES: Mutex<Option<Stores>> = Mutex::new(None);

struct Stores {
    mutants: Store<MutantDeployment>,
    mutations: Store<Mutation>,
}

/// Hands the controllers' caches to the scrape handler.
pub fn track(mutants: Store<MutantDeployment>, mutations: Store<Mutation>) {
    *STORES.lock().unwrap_or_else(|e| e.into_inner()) = Some(Stores { mutants, mutations });
}

/// Sets the gauges from the resources as the controllers last saw them.
fn record_managed(mutants: &[impl AsRef<MutantDeployment>], mutations: &[impl AsRef<Mutation>]) {
    let m = &*METRICS;
    m.managed_objects.with_label_values(&[KIND_MUTANT]).set(mutants.len() as i64);
    m.managed_objects.with_label_values(&[KIND_MUTATION]).set(mutations.len() as i64);

    let mutant_variants: usize = mutants.iter().map(|md| md.as_ref().spec.mutations.len()).sum();
    let mutation_variants: usize = mutations.iter().map(|mu| mu.as_ref().spec.variants.len()).sum();
    m.variants.with_label_values(&[KIND_MUTANT]).set(mutant_variants as i64);
    m.variants.with_label_values(&[KIND_MUTATION]).set(mutation_variants as i64);

    let mut by_state = [0i64; VARIANT_STATES.len()];
    for status in mutations.iter().filter_map(|mu| mu.as_ref().status.as_ref()) {
        for v in &status.variants {
            if let Some(i) = VARIANT_STATES.iter().position(|s| *s == v.state) {
                by_state[i] += 1;
            }
        }
    }
    for (state, n) in VARIANT_STATES.iter().zip(by_state) {
        m.mutation_variants.with_label_values(&[state_label(*state)]).set(n);
    }
}

fn refresh() {
    let guard = STORES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(stores) = guard.as_ref() {
        record_managed(&stores.mutants.state(), &stores.mutations.state());
    }
}

/// Times a reconcile and counts it by outcome.
pub async fn instrument<T, E>(kind: &'static str, reconcile: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    let start = Instant::now();
    let res = reconcile.await;
    let outcome = if res.is_ok() { "ok" } else { "error" };
    let m = &*METRICS;
    m.reconcile_duration.with_label_values(&[kind, outcome]).observe(start.elapsed().as_secs_f64());
    m.reconciliations.with_label_values(&[kind, outcome]).inc();
    res
}

/// Counts a failed reconcile; `error` is a short, bounded label.
pub fn reconcile_error(kind: &str, error: &str) {
    METRICS.reconcile_errors.with_label_values(&[kind, error]).inc();
}

/// Counts an error the controller's stream reported other than a failed
/// reconcile, which [`reconcile_error`] has already counted.
pub fn controller_error(kind: &str, error: &str) {
    METRICS.controller_errors.with_label_values(&[kind, error]).inc();
}

/// The text exposition of every metric, refreshed from the caches first.
pub fn render() -> Result<Vec<u8>, prometheus::Error> {
    refresh();
    let mut body = Vec::new();
    TextEncoder::new().encode(&METRICS.registry.gather(), &mut body)?;
    Ok(body)
}

async fn metrics() -> impl IntoResponse {
    match render() {
        Ok(body) => ([(header::CONTENT_TYPE, TextEncoder::new().format_type().to_string())], body).into_response(),
        Err(e) => {
            error!("metrics: encoding: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The metrics listener's router.
pub fn router() -> Router {
    Router::new().route("/metrics", get(metrics))
}

/// Serves [`router`] on `listener` until the process exits.
pub async fn serve(listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{MutationSpec, MutationStatus, VariantStatus};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn text() -> String {
        String::from_utf8(render().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn serves_metrics_over_http() {
        instrument(KIND_MUTATION, async { Ok::<_, ()>(()) }).await.unwrap();
        // The real listener, over a real socket.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        conn.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut body = String::new();
        conn.read_to_string(&mut body).await.unwrap();
        assert!(body.starts_with("HTTP/1.1 200"), "{body}");
        assert!(body.to_ascii_lowercase().contains("content-type: text/plain; version=0.0.4"), "{body}");
        for name in [
            "mutant_operator_build_info",
            "mutant_operator_reconciliations_total",
            "mutant_operator_reconcile_duration_seconds_bucket",
            "mutant_operator_managed_objects",
            "mutant_operator_mutation_variants",
        ] {
            assert!(body.contains(name), "{name} missing from:\n{body}");
        }
        #[cfg(target_os = "linux")]
        assert!(body.contains("process_resident_memory_bytes"), "{body}");
    }

    #[tokio::test]
    async fn counts_reconciles_and_errors() {
        // Its own kind label, so the parallel test cannot disturb the count.
        const KIND: &str = "TestKind";
        instrument(KIND, async { Ok::<_, ()>(()) }).await.unwrap();
        instrument(KIND, async { Err::<(), _>("boom") }).await.unwrap_err();
        reconcile_error(KIND, "api");
        let t = text();
        assert!(t.contains(r#"mutant_operator_reconciliations_total{kind="TestKind",outcome="ok"} 1"#), "{t}");
        assert!(t.contains(r#"mutant_operator_reconciliations_total{kind="TestKind",outcome="error"} 1"#), "{t}");
        assert!(t.contains(r#"mutant_operator_reconcile_duration_seconds_count{kind="TestKind",outcome="ok"} 1"#));
        assert!(t.contains(r#"mutant_operator_reconcile_errors_total{error="api",kind="TestKind"} 1"#), "{t}");
    }

    #[test]
    fn managed_gauges_follow_the_resources() {
        let variant = |state| VariantStatus::rejected("x", state, String::new());
        let spec: MutationSpec = serde_json::from_value(serde_json::json!({
            "deployment": {},
            "variants": [
                {"label": "a", "instances": 1},
                {"label": "b", "instances": 1},
                {"label": "c", "instances": 1},
            ],
        }))
        .unwrap();
        let mut m = Mutation::new("web", spec);
        m.status = Some(MutationStatus {
            variants: vec![variant(VariantState::Ready), variant(VariantState::Ready), variant(VariantState::Conflict)],
            ..MutationStatus::default()
        });
        record_managed(&[] as &[std::sync::Arc<MutantDeployment>], &[std::sync::Arc::new(m)]);
        let t = text();
        assert!(t.contains(r#"mutant_operator_managed_objects{kind="Mutation"} 1"#), "{t}");
        assert!(t.contains(r#"mutant_operator_managed_objects{kind="MutantDeployment"} 0"#), "{t}");
        assert!(t.contains(r#"mutant_operator_variants{kind="Mutation"} 3"#), "{t}");
        assert!(t.contains(r#"mutant_operator_mutation_variants{state="Ready"} 2"#), "{t}");
        assert!(t.contains(r#"mutant_operator_mutation_variants{state="Conflict"} 1"#), "{t}");
        assert!(t.contains(r#"mutant_operator_mutation_variants{state="Invalid"} 0"#), "{t}");
    }
}
