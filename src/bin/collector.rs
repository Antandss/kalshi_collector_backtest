use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

use trading::db;
use trading::kalshi::{auth::KalshiAuth, ws};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Load .env
    dotenvy::dotenv().ok();

    let key_id   = std::env::var("KALSHI_KEY_ID")  .context("KALSHI_KEY_ID not set in .env")?;
    let key_file = std::env::var("KALSHI_KEY_FILE") .context("KALSHI_KEY_FILE not set in .env")?;

    // Resolve key file relative to project root (same dir as .env).
    let project_root = concat!(env!("CARGO_MANIFEST_DIR"), "/");
    let key_path = if std::path::Path::new(&key_file).is_absolute() {
        key_file
    } else {
        format!("{}{}", project_root, key_file)
    };

    let auth = Arc::new(KalshiAuth::load(key_id, &key_path)?);
    tracing::info!("Loaded Kalshi key: {}", auth.key_id);

    let db_path = concat!(env!("CARGO_MANIFEST_DIR"), "/trading.db");
    let pool = Arc::new(db::connect(db_path).await?);
    let abs = std::fs::canonicalize(db_path).unwrap_or_default();
    tracing::info!("Database: {}", abs.display());

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // Run WebSocket collector — reconnects automatically on failure.
    ws::run(auth, pool, client).await;

    Ok(())
}
