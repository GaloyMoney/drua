use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider},
    Resource,
};
use opentelemetry_semantic_conventions::attribute::SERVICE_NAMESPACE;
use tracing::{error, error_span};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer as _};

pub struct TracingConfig {
    pub service_name: String,
}

#[derive(thiserror::Error, Debug)]
pub enum TracingError {
    #[error("TracingError - OtelSdk: {0}")]
    OtelSdk(#[from] opentelemetry_sdk::error::OTelSdkError),
}

#[derive(Clone)]
struct TracerHandle {
    provider: Arc<SdkTracerProvider>,
    shutdown_called: Arc<AtomicBool>,
}

static TRACER_HANDLE: Mutex<Option<TracerHandle>> = Mutex::new(None);

fn otel_sdk_disabled() -> bool {
    std::env::var("OTEL_SDK_DISABLED")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false)
}

pub fn init_tracer(config: TracingConfig) -> anyhow::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());

    let telemetry = if otel_sdk_disabled() {
        None
    } else {
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:4317".to_string());

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;

        let batch_config = BatchConfigBuilder::default()
            .with_max_queue_size(65536)
            .with_max_export_batch_size(2048)
            .build();

        let batch_processor = BatchSpanProcessor::builder(exporter)
            .with_batch_config(batch_config)
            .build();

        let provider = SdkTracerProvider::builder()
            .with_resource(telemetry_resource(&config))
            .with_span_processor(batch_processor)
            .with_sampler(Sampler::AlwaysOn)
            .build();

        let provider_arc = Arc::new(provider);

        // Store handle in global state for graceful shutdown
        let handle = TracerHandle {
            provider: Arc::clone(&provider_arc),
            shutdown_called: Arc::new(AtomicBool::new(false)),
        };
        {
            let mut guard = TRACER_HANDLE.lock().expect("Failed to lock tracer handle");
            *guard = Some(handle);
        }

        global::set_tracer_provider((*provider_arc).clone());
        let tracer = provider_arc.tracer("galoy-agents-tracer");

        // Build separate filter for OTel that excludes tokio/runtime
        let otel_filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new("info"))
            .expect("default EnvFilter should be valid")
            .add_directive("tokio=off".parse().expect("tokio=off directive is valid"))
            .add_directive(
                "runtime=off"
                    .parse()
                    .expect("runtime=off directive is valid"),
            );

        Some(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(otel_filter),
        )
    };

    // Build separate filter for fmt_layer that excludes tokio/runtime from stdout
    let fmt_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("default EnvFilter should be valid")
        .add_directive("tokio=off".parse().expect("tokio=off directive is valid"))
        .add_directive(
            "runtime=off"
                .parse()
                .expect("runtime=off directive is valid"),
        );

    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_filter(fmt_filter);

    let base_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("default EnvFilter should be valid");

    tracing_subscriber::registry()
        .with(base_filter)
        .with(fmt_layer)
        .with(telemetry)
        .init();

    setup_panic_hook();

    Ok(())
}

pub fn shutdown_tracer() -> Result<(), TracingError> {
    let handle = {
        let guard = TRACER_HANDLE.lock().expect("Failed to lock tracer handle");
        guard.clone()
    };

    if let Some(handle) = handle {
        perform_shutdown(handle)?;
    } else {
        eprintln!("No tracer handle found during shutdown");
    }

    Ok(())
}

fn perform_shutdown(handle: TracerHandle) -> Result<(), TracingError> {
    if handle
        .shutdown_called
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        handle.provider.force_flush()?;
        handle.provider.shutdown()?;
    }
    Ok(())
}

fn telemetry_resource(config: &TracingConfig) -> Resource {
    Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes([KeyValue::new(SERVICE_NAMESPACE, "galoy-agents")])
        .build()
}

fn setup_panic_hook() {
    let default_panic = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        let span = error_span!("panic", panic_type = "unhandled");
        let _guard = span.enter();

        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic payload".to_string()
        };

        error!(
            target: "panic",
            panic_message = %message,
            panic_location = ?panic_info.location(),
            panic_thread = ?std::thread::current().name(),
            panic_backtrace = ?std::backtrace::Backtrace::capture(),
            "Unhandled panic in application"
        );

        default_panic(panic_info);
    }));
}
