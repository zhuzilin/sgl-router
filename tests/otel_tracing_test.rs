mod common;

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::StatusCode,
    routing::post,
};
use common::mock_worker::{HealthStatus, MockWorker, MockWorkerConfig, WorkerType};
use portpicker::pick_unused_port;
use serde_json::json;
use serial_test::serial;
use smg::{
    config::{RouterConfig, TraceConfig},
    core::Job,
    observability::{logging, otel_trace},
    routers::RouterFactory,
};
use tokio::sync::oneshot;
use tower::ServiceExt;
use tracing::info_span;

/// A simple HTTP-based mock OTLP collector that counts received trace exports.
#[derive(Clone)]
struct TestOtelCollector {
    span_count: Arc<AtomicUsize>,
}

impl TestOtelCollector {
    fn new() -> Self {
        Self {
            span_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn get_span_count(&self) -> usize {
        self.span_count.load(Ordering::SeqCst)
    }
}

async fn start_collector(
    port: u16,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<TestOtelCollector, Box<dyn std::error::Error>> {
    let collector = TestOtelCollector::new();
    let collector_clone = collector.clone();

    let app = axum::Router::new().route(
        "/v1/traces",
        post(move |body: Bytes| {
            let collector = collector_clone.clone();
            async move {
                // Each POST to /v1/traces is one export batch; count it as at least 1 span
                let count = body.len().max(1);
                collector.span_count.fetch_add(1, Ordering::SeqCst);
                let _ = count;
                StatusCode::OK
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                shutdown_rx.await.ok();
            })
            .await
            .ok();
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    Ok(collector)
}

#[tokio::test]
#[serial]
async fn test_router_with_tracing() {
    // 1. Start the OTLP collector
    let port = pick_unused_port().expect("Failed to pick unused port");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let collector = start_collector(port, shutdown_rx)
        .await
        .expect("Failed to start collector");
    let collector_endpoint = format!("http://0.0.0.0:{}", port);
    println!("OTLP Collector started on: {}", collector_endpoint);

    // 2. create the mock worker
    let mut mock_worker = MockWorker::new(MockWorkerConfig {
        port: 0,
        worker_type: WorkerType::Regular,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 0,
        fail_rate: 0.0,
    });

    let worker_url = mock_worker.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("Mock worker started on: {}", worker_url);

    // 3. create router config and enable tracing
    let router_config = RouterConfig::builder()
        .regular_mode(vec![worker_url.clone()])
        .random_policy()
        .host("0.0.0.0")
        .port(0)
        .max_payload_size(256 * 1024 * 1024)
        .request_timeout_secs(60)
        .worker_startup_timeout_secs(1)
        .worker_startup_check_interval_secs(1)
        .max_concurrent_requests(64)
        .queue_timeout_secs(60)
        .enable_trace(&collector_endpoint)
        .build_unchecked();

    // 4. Initialize the OTLP client (check if already initialized by another test)
    let otel_initialized_by_this_test = if !otel_trace::is_otel_enabled() {
        let init_result = otel_trace::otel_tracing_init(true, Some(&collector_endpoint));
        assert!(
            init_result.is_ok(),
            "Failed to initialize OTEL: {:?}",
            init_result.err()
        );
        println!("OpenTelemetry initialized successfully");
        true
    } else {
        println!(
            "OpenTelemetry already initialized by previous test (spans will go to that collector)"
        );
        false
    };

    let trace_config = TraceConfig {
        enable_trace: true,
        otlp_traces_endpoint: collector_endpoint.clone(),
    };
    let _log_guard = logging::init_logging(
        logging::LoggingConfig {
            level: tracing::Level::INFO,
            json_format: false,
            log_dir: None,
            colorize: false,
            log_file_name: "test-otel".to_string(),
            log_targets: Some(vec!["smg".to_string()]),
        },
        Some(trace_config),
    );
    println!("Logging initialized with OTEL layer");

    // 5. Create a span and sleep for a while
    let _span = info_span!(target: "smg::otel-trace", "test_router_with_tracing");
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(_span);

    // 6. create app context and router
    let app_context = common::create_test_context(router_config.clone()).await;

    // 7. initialize worker
    let job_queue = app_context
        .worker_job_queue
        .get()
        .expect("JobQueue should be initialized");

    let job = Job::InitializeWorkersFromConfig {
        router_config: Box::new(router_config.clone()),
    };

    job_queue
        .submit(job)
        .await
        .expect("Failed to submit worker init job");

    // 8. wait for worker initialization
    tokio::time::sleep(Duration::from_millis(1000)).await;
    println!("Workers initialized");

    // 9. create router
    let router = RouterFactory::create_router(&app_context)
        .await
        .expect("Failed to create router");

    println!("Router created");

    // 10. create app (middleware::create_logging_layer() will use the already initialized OTEL layer)
    let app =
        common::test_app::create_test_app_with_context(Arc::from(router), app_context.clone());

    println!("App created with logging middleware");

    // 10. send request
    let request_body = json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello, test OpenTelemetry tracing!"}
        ],
        "temperature": 0.7,
        "max_tokens": 50
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    println!("Sending request to router...");
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK, "Request should succeed");

    println!("Request completed successfully");
    drop(response);

    // 11. Wait for spans to be exported
    match otel_trace::flush_spans_async().await {
        Ok(_) => println!("Spans flushed successfully"),
        Err(e) => println!("Failed to flush spans: {:?}", e),
    }

    // 12. Verify that the spans were exported to the collector
    let span_count = collector.get_span_count();
    println!("Total spans received by collector: {}", span_count);

    // Only assert span count if we initialized OTEL with our own collector
    // When OTEL was pre-initialized by another test, spans go to that collector instead
    if otel_initialized_by_this_test {
        assert!(
            span_count == 2,
            "Expected to receive at least 2 span, but got {}. \
            This indicates that tracing data is not being exported to the OTLP collector.",
            span_count
        );
        println!("Test passed! Collector received {} spans", span_count);
    } else {
        println!(
            "Skipping span count assertion - OTEL was pre-initialized by another test. \
            Spans went to that collector. Received {} spans on this test's collector.",
            span_count
        );
    }

    // 13. cleanup
    let _ = shutdown_tx.send(());
    mock_worker.stop().await;

    println!("Cleanup completed");
}
