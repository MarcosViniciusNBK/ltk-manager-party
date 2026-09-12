mod audit;
mod auth;
mod config;
mod error;
mod manifest;
mod rate_limit;
mod routes;
mod state;
mod storage;

use std::path::PathBuf;
use std::time::Duration;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use config::ServerConfig;
use sqlx::postgres::PgPoolOptions;
use state::AppState;
use storage::StorageManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ltk_room_server=debug,tower_http=info,axum=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting LeagueToolkit Room Synchronization Server...");

    let config = ServerConfig::from_env().unwrap_or_else(|err| {
        tracing::warn!(error = %err, "Configuration fallback: using default configuration");
        ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 3000,
            database_url: "postgres://postgres:postgres@localhost:5432/ltk_rooms".to_string(),
            max_connections: 20,
        }
    });

    info!(
        host = %config.host,
        port = %config.port,
        "Connecting to database..."
    );

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await?;

    info!("Running pending database migrations...");
    sqlx::migrate!("./migrations").run(&pool).await?;
    info!("Database migrations applied successfully.");

    // Initialize Content-Addressed Storage
    let storage_dir = std::env::var("STORAGE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                std::env::temp_dir().join("ltk-storage")
            } else {
                PathBuf::from("/data/blobs")
            }
        });
    let storage_secret = std::env::var("STORAGE_SECRET").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "STORAGE_SECRET must be configured",
        )
    })?;
    if storage_secret.len() < 32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "STORAGE_SECRET must contain at least 32 characters",
        )
        .into());
    }
    let storage_secret = storage_secret.into_bytes();
    let public_url = std::env::var("PUBLIC_SERVER_URL")
        .unwrap_or_else(|_| "https://mag.horuzprod.com/ltk-rooms".to_string());
    let updates_dir = std::env::var("UPDATES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/updates"));
    std::fs::create_dir_all(&updates_dir)?;

    info!(path = ?storage_dir, url = %public_url, "Initializing Content-Addressed Storage...");
    let storage = StorageManager::new(storage_dir, storage_secret, public_url.clone())?;

    // Background maintenance worker: prune expired rooms periodically
    let pool_cleanup = pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        loop {
            interval.tick().await;
            let res = sqlx::query("DELETE FROM rooms WHERE expires_at < NOW()")
                .execute(&pool_cleanup)
                .await;
            match res {
                Ok(r) if r.rows_affected() > 0 => {
                    info!(
                        cleaned_rooms = r.rows_affected(),
                        "Cleaned up expired rooms"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Room cleanup background task error");
                }
                _ => {}
            }
        }
    });

    // Background maintenance worker: prune orphan blobs older than 48 hours
    let pool_orphan = pool.clone();
    let storage_orphan = storage.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        loop {
            interval.tick().await;
            let orphans: Vec<String> = sqlx::query_scalar(
                "SELECT rb.content_hash FROM room_blobs rb \
                 WHERE rb.created_at < NOW() - INTERVAL '48 hours' \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM room_manifests rm \
                     WHERE rm.manifest_json::text LIKE '%' || rb.content_hash || '%' \
                 ) \
                 LIMIT 50",
            )
            .fetch_all(&pool_orphan)
            .await
            .unwrap_or_default();

            for hash in orphans {
                let path = storage_orphan.blob_path(&hash);
                let _ = std::fs::remove_file(path);
                let _ = sqlx::query("DELETE FROM room_blobs WHERE content_hash = $1")
                    .bind(&hash)
                    .execute(&pool_orphan)
                    .await;
                info!(orphan_hash = %hash, "Reclaimed orphan blob storage");
            }
        }
    });

    let state = AppState::new(pool, storage, updates_dir, public_url);

    let app = routes::create_router(state).layer(TraceLayer::new_for_http());

    let addr = config.socket_addr();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "Listening for HTTP, WebSocket, and Blob Transfer connections");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shutdown cleanly.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT (Ctrl+C), shutting down..."),
        _ = terminate => info!("Received SIGTERM, shutting down..."),
    }
}
