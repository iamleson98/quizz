//! Observability wiring:
//!  * `tracing` structured JSON (or pretty) logs with RUST_LOG/EnvFilter,
//!  * optional OpenTelemetry OTLP/gRPC trace export (tracing-opentelemetry),
//!  * Prometheus metrics recorder served at `/metrics`,
//!  * an axum middleware recording per-route HTTP counters/histograms.

use crate::config::Config;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::time::Instant;

/// Latency buckets (seconds) tuned for a fast local-database service.
const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Install the global Prometheus recorder (once per process) and return a
/// render handle. Safe to call multiple times (e.g. from tests).
pub fn init_metrics() -> PrometheusHandle {
    static HANDLE: std::sync::OnceLock<PrometheusHandle> = std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("http".into()),
                    LATENCY_BUCKETS,
                )
                .expect("valid latency buckets")
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("tx".into()),
                    LATENCY_BUCKETS,
                )
                .expect("valid latency buckets")
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Prefix("quiz_question".into()),
                    LATENCY_BUCKETS,
                )
                .expect("valid latency buckets")
                .install_recorder()
                .expect("install prometheus recorder")
        })
        .clone()
}

/// Holds the OTel tracer provider so spans can be flushed on shutdown.
pub struct OtelGuard {
    provider: Option<SdkTracerProvider>,
}

impl OtelGuard {
    pub fn shutdown(self) {
        if let Some(provider) = self.provider
            && let Err(e) = provider.shutdown()
        {
            tracing::warn!(error = %e, "failed to flush OTel spans");
        }
    }
}

/// Initialize the tracing subscriber with EnvFilter, a formatting layer and,
/// when configured, an OpenTelemetry OTLP exporter layer.
/// Must be called once at startup, before meaningful `tracing` calls.
pub fn init_tracing(cfg: &Config) -> OtelGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{Registry, filter::EnvFilter, fmt};

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = Registry::default().with(env_filter);

    let provider = build_provider(cfg);

    match (provider.as_ref(), cfg.log_format.as_str()) {
        (Some(provider), "pretty") => {
            let otel = tracing_opentelemetry::layer().with_tracer(provider.tracer("quiz-service"));
            registry.with(otel).with(fmt::layer().pretty()).init();
        }
        (Some(provider), _) => {
            let otel = tracing_opentelemetry::layer().with_tracer(provider.tracer("quiz-service"));
            registry.with(otel).with(fmt::layer().json()).init();
        }
        (None, "pretty") => {
            registry.with(fmt::layer().pretty()).init();
        }
        (None, _) => {
            registry.with(fmt::layer().json()).init();
        }
    }

    if let Some(endpoint) = &cfg.otel_endpoint {
        tracing::info!(%endpoint, "OpenTelemetry OTLP tracing enabled");
    }

    OtelGuard { provider }
}

fn build_provider(cfg: &Config) -> Option<SdkTracerProvider> {
    use opentelemetry_otlp::WithExportConfig;

    let endpoint = cfg.otel_endpoint.as_deref()?;
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("build OTLP span exporter");

    Some(
        SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                Resource::builder()
                    .with_service_name("quiz-service")
                    .build(),
            )
            .build(),
    )
}

/// axum middleware: per-route HTTP request counters and latency histograms.
pub async fn http_metrics_middleware(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let res = next.run(req).await;
    let status = res.status().as_u16().to_string();
    let elapsed = start.elapsed().as_secs_f64();

    metrics::counter!(
        "http_requests_total",
        "method" => method,
        "route" => route.clone(),
        "status" => status
    )
    .increment(1);
    metrics::histogram!("http_request_duration_seconds", "route" => route).record(elapsed);

    res
}
