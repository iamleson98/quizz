//! quiz-service entrypoint.

use quiz_service::config::Config;
use quiz_service::push::Pusher;
use quiz_service::state::AppState;
use quiz_service::{retention, server, snapshot, telemetry};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::info;

/// How long the service waits for rooms to close on shutdown.
const SHUTDOWN_ROOMS_WITHIN: Duration = Duration::from_secs(5);
/// How long the final push of lesson-time spans may take.
const SHUTDOWN_PUSH_WITHIN: Duration = Duration::from_secs(3);

fn main() {
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("quiz-service cannot start: {e}");
            std::process::exit(2);
        }
    };
    // Few threads: on its node the service shares cores with the SFU.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.worker_threads)
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(cfg)) {
        eprintln!("quiz-service failed: {e}");
        std::process::exit(1);
    }
}

async fn bind(addr: &str) -> Result<TcpListener, String> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))
}

async fn run(cfg: Config) -> Result<(), String> {
    let metrics = telemetry::init_metrics();
    let otel = telemetry::init_tracing(&cfg);

    let state = AppState::connect(cfg.clone())
        .await
        .map_err(|e| format!("cannot open the database: {e}"))?;
    info!(
        sqlite_version = %state.sqlite_version,
        tx_mode = %cfg.tx_mode.as_str(),
        workers = cfg.worker_threads,
        vfs = %cfg.vfs,
        database = ?cfg.database_path,
        "quiz-service starting"
    );

    let public = bind(&cfg.listen_addr).await?;
    let internal = bind(&cfg.internal_addr).await?;

    retention::spawn(state.clone());
    snapshot::spawn(state.clone());
    let pusher = Pusher::new(&state);
    if let Some(p) = Pusher::new(&state) {
        tokio::spawn(p.run());
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let serve = |listener: TcpListener,
                 app: axum::Router,
                 mut stop: tokio::sync::watch::Receiver<bool>| async move {
        let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
            let _ = tcp.set_nodelay(true);
        });
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop.wait_for(|s| *s).await;
            })
            .await
    };
    info!(public = %cfg.listen_addr, internal = %cfg.internal_addr, "listening");
    let public_task = tokio::spawn(serve(
        public,
        server::public_router(state.clone()),
        stop_rx.clone(),
    ));
    let internal_task = tokio::spawn(serve(
        internal,
        server::internal_router(state.clone(), metrics),
        stop_rx,
    ));

    shutdown_signal().await;
    // Games end and their spans close before the sockets and the pusher go.
    state.rooms.shutdown(SHUTDOWN_ROOMS_WITHIN).await;
    if let Some(p) = &pusher {
        let _ = tokio::time::timeout(SHUTDOWN_PUSH_WITHIN, p.flush_once()).await;
    }
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = public_task.await;
        let _ = internal_task.await;
    })
    .await;

    info!("shutdown complete, flushing telemetry");
    otel.shutdown();
    Ok(())
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
