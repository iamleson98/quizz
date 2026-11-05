//! quiz-service entrypoint.

use quiz_service::config::Config;
use quiz_service::graphql;
use quiz_service::state::AppState;
use quiz_service::telemetry;
use tracing::info;

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();
    let metrics = telemetry::init_metrics();
    let otel = telemetry::init_tracing(&cfg);

    let state = match AppState::connect(cfg.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to initialize database: {e}");
            std::process::exit(1);
        }
    };

    info!(
        sqlite_version = %state.sqlite_version,
        tx_mode = %cfg.tx_mode.as_str(),
        commit_mutex = cfg.commit_mutex,
        pool_max = cfg.pool_max,
        vfs = %cfg.vfs,
        database = ?cfg.database_path,
        "quiz-service starting (custom begin-concurrent SQLite statically linked)"
    );

    let app = graphql::build_router(state.clone(), metrics);

    let listener = match tokio::net::TcpListener::bind(&cfg.listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {}: {e}", cfg.listen_addr);
            std::process::exit(1);
        }
    };
    info!(addr = %cfg.listen_addr, playground = "http://localhost:8080/playground", "listening");

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }

    info!("shutdown complete, flushing telemetry");
    otel.shutdown();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
