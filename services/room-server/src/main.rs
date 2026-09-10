mod auth;
mod config;
mod error;
mod rate_limit;
mod routes;
mod state;

use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use config::ServerConfig;
use sqlx::postgres::PgPoolOptions;
use state::AppState;

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

    let state = AppState::new(pool);

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = routes::create_router(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ));

    let addr = config.socket_addr();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "Listening for HTTP and WebSocket connections");

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
