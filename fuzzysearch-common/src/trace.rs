pub fn configure_tracing() {
    use opentelemetry::KeyValue;
    use tracing_subscriber::layer::SubscriberExt;

    let env = std::env::var("ENVIRONMENT");
    let env = if let Ok(env) = env.as_ref() {
        env.as_str()
    } else if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    opentelemetry::global::set_text_map_propagator(opentelemetry_jaeger::Propagator::new());

    let tracer = opentelemetry_jaeger::new_pipeline()
        .with_agent_endpoint(std::env::var("JAEGER_COLLECTOR").expect("Missing JAEGER_COLLECTOR"))
        .with_service_name(env!("CARGO_CRATE_NAME"))
        .with_tags(vec![
            KeyValue::new("environment", env.to_owned()),
            KeyValue::new("version", env!("CARGO_PKG_VERSION")),
        ])
        .install_batch(opentelemetry::runtime::Tokio)
        .unwrap();

    let trace = tracing_opentelemetry::layer().with_tracer(tracer);
    let env_filter = tracing_subscriber::EnvFilter::from_default_env();

    if matches!(std::env::var("LOG_FMT").as_deref(), Ok("json")) {
        let subscriber = tracing_subscriber::fmt::layer()
            .json()
            .with_timer(tracing_subscriber::fmt::time::ChronoUtc::rfc3339())
            .with_target(true);
        let subscriber = tracing_subscriber::Registry::default()
            .with(env_filter)
            .with(trace)
            .with(subscriber);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    } else {
        let subscriber = tracing_subscriber::fmt::layer();
        let subscriber = tracing_subscriber::Registry::default()
            .with(env_filter)
            .with(trace)
            .with(subscriber);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    }
}

async fn metrics(
    _: hyper::Request<hyper::Body>,
) -> Result<hyper::Response<hyper::Body>, std::convert::Infallible> {
    use hyper::{Body, Response};
    use prometheus::{Encoder, TextEncoder};

    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();

    let metric_families = prometheus::gather();
    encoder.encode(&metric_families, &mut buffer).unwrap();

    Ok(Response::new(Body::from(buffer)))
}

pub async fn serve_metrics() {
    use hyper::{
        service::{make_service_fn, service_fn},
        Server,
    };
    use std::convert::Infallible;
    use std::net::SocketAddr;

    let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(metrics)) });

    let addr: SocketAddr = std::env::var("METRICS_HOST")
        .expect("Missing METRICS_HOST")
        .parse()
        .expect("Invalid METRICS_HOST");

    let server = Server::bind(&addr).serve(make_svc);

    tokio::spawn(async move {
        server.await.expect("Metrics server error");
    });
}
