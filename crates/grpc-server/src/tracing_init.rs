use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    runtime,
    trace::{self as sdktrace, Sampler},
    Resource,
};
use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, SERVICE_VERSION};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub struct TracingGuard {
    otel_installed: bool,
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if self.otel_installed {
            global::shutdown_tracer_provider();
        }
    }
}

/// Initialise the global tracing subscriber. When `OTEL_EXPORTER_OTLP_ENDPOINT`
/// is set, spans are additionally exported to an OTLP/gRPC collector (Tempo in
/// our compose stack); otherwise only the stdout log layer is wired.
///
/// `LOG_FORMAT=json` picks the JSON fmt layer; anything else keeps the
/// human-readable pretty output.
pub fn init() -> TracingGuard {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let json_fmt = matches!(std::env::var("LOG_FORMAT").as_deref(), Ok("json"));
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty());

    let otel_layer = otlp_endpoint.as_deref().map(|endpoint| {
        let service_name = std::env::var("OTEL_SERVICE_NAME")
            .unwrap_or_else(|_| "aether-rust".to_string());
        let resource = Resource::new(vec![
            KeyValue::new(SERVICE_NAME, service_name),
            KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
        ]);

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .expect("failed to build OTLP span exporter");

        let provider = sdktrace::TracerProvider::builder()
            .with_batch_exporter(exporter, runtime::Tokio)
            .with_resource(resource)
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
            .build();

        let tracer = provider.tracer("aether-grpc-server");
        global::set_tracer_provider(provider);
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    let otel_installed = otel_layer.is_some();

    let fmt_layer_json = if json_fmt {
        Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(false),
        )
    } else {
        None
    };
    let fmt_layer_pretty = if json_fmt {
        None
    } else {
        Some(tracing_subscriber::fmt::layer())
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer_json)
        .with(fmt_layer_pretty)
        .with(otel_layer)
        .init();

    if otel_installed {
        if let Some(endpoint) = otlp_endpoint {
            tracing::info!(endpoint, "OTLP tracing exporter installed");
        }
    }

    TracingGuard { otel_installed }
}
