//! SQLite persistence layer via sqlx.

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool, sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions}};
use std::str::FromStr;

use crate::models::{Market, PriceTick};

pub async fn connect(db_path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path))?
        .journal_mode(SqliteJournalMode::Wal)
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .context("opening SQLite database")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS markets (
            id          TEXT PRIMARY KEY,
            question    TEXT NOT NULL,
            team_a      TEXT NOT NULL,
            team_b      TEXT NOT NULL,
            token_yes   TEXT NOT NULL,
            token_no    TEXT NOT NULL,
            start_time  INTEGER,
            end_time    INTEGER,
            active      INTEGER NOT NULL DEFAULT 1,
            created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now') * 1000)
        )",
    )
    .execute(&pool)
    .await
    .context("creating markets table")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS price_ticks (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id   TEXT NOT NULL REFERENCES markets(id),
            token_id    TEXT NOT NULL,
            side        TEXT NOT NULL,
            bid         REAL,
            ask         REAL,
            price       REAL NOT NULL,
            timestamp   INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .context("creating price_ticks table")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ticks_market_ts ON price_ticks(market_id, timestamp)",
    )
    .execute(&pool)
    .await
    .context("creating index")?;

    Ok(pool)
}

// ── Markets ───────────────────────────────────────────────────────────────────

pub async fn upsert_market(pool: &SqlitePool, m: &Market) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO markets (id, question, team_a, team_b, token_yes, token_no, start_time, end_time, active)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            active   = excluded.active,
            end_time = excluded.end_time
        "#,
    )
    .bind(&m.id)
    .bind(&m.question)
    .bind(&m.team_a)
    .bind(&m.team_b)
    .bind(&m.token_yes)
    .bind(&m.token_no)
    .bind(m.start_time)
    .bind(m.end_time)
    .bind(m.active as i64)
    .execute(pool)
    .await
    .context("upserting market")?;

    Ok(())
}

pub async fn load_active_markets(pool: &SqlitePool) -> Result<Vec<Market>> {
    let rows = sqlx::query(
        "SELECT id, question, team_a, team_b, token_yes, token_no, start_time, end_time, active
         FROM markets WHERE active = 1",
    )
    .fetch_all(pool)
    .await
    .context("loading active markets")?;

    rows.iter().map(row_to_market).collect()
}

pub async fn load_all_markets(pool: &SqlitePool) -> Result<Vec<Market>> {
    let rows = sqlx::query(
        "SELECT id, question, team_a, team_b, token_yes, token_no, start_time, end_time, active
         FROM markets",
    )
    .fetch_all(pool)
    .await
    .context("loading all markets")?;

    rows.iter().map(row_to_market).collect()
}

pub async fn mark_market_inactive(pool: &SqlitePool, market_id: &str) -> Result<()> {
    sqlx::query("UPDATE markets SET active = 0 WHERE id = ?")
        .bind(market_id)
        .execute(pool)
        .await
        .context("marking market inactive")?;
    Ok(())
}

fn row_to_market(r: &sqlx::sqlite::SqliteRow) -> Result<Market> {
    Ok(Market {
        id: r.try_get("id")?,
        question: r.try_get("question")?,
        team_a: r.try_get("team_a")?,
        team_b: r.try_get("team_b")?,
        token_yes: r.try_get("token_yes")?,
        token_no: r.try_get("token_no")?,
        start_time: r.try_get("start_time")?,
        end_time: r.try_get("end_time")?,
        active: r.try_get::<i64, _>("active")? != 0,
    })
}

// ── Price ticks ───────────────────────────────────────────────────────────────

pub async fn insert_tick(pool: &SqlitePool, tick: &PriceTick) -> Result<()> {
    let side = tick.side.to_string();
    let ts = tick.timestamp.timestamp_millis();

    sqlx::query(
        "INSERT INTO price_ticks (market_id, token_id, side, bid, ask, price, timestamp)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&tick.market_id)
    .bind(&tick.token_id)
    .bind(&side)
    .bind(tick.bid)
    .bind(tick.ask)
    .bind(tick.price)
    .bind(ts)
    .execute(pool)
    .await
    .context("inserting price tick")?;

    Ok(())
}

/// Load YES-side price history for a market ordered by time.
/// Returns (timestamp_ms, bid, ask, mid).
pub async fn load_yes_ticks(
    pool: &SqlitePool,
    market_id: &str,
) -> Result<Vec<(i64, f64, f64, f64)>> {
    let rows = sqlx::query(
        "SELECT timestamp, bid, ask, price FROM price_ticks
         WHERE market_id = ? AND side = 'yes'
         ORDER BY timestamp ASC",
    )
    .bind(market_id)
    .fetch_all(pool)
    .await
    .context("loading yes ticks")?;

    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("timestamp")?,
                r.try_get::<Option<f64>, _>("bid")?.unwrap_or(r.try_get("price")?),
                r.try_get::<Option<f64>, _>("ask")?.unwrap_or(r.try_get("price")?),
                r.try_get("price")?,
            ))
        })
        .collect()
}