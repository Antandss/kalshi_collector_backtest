use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::time::interval;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::auth::KalshiAuth;
use super::rest::fetch_cs2_markets;
use crate::db;
use crate::models::{PriceTick, Side};

const WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
const WS_PATH: &str = "/trade-api/ws/v2";
const MARKET_REFRESH_SECS: u64 = 300;

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Subscribe {
    id: u64,
    cmd: &'static str,
    params: SubscribeParams,
}

#[derive(Serialize)]
struct SubscribeParams {
    channels: Vec<&'static str>,
    market_tickers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WsEnvelope {
    #[serde(rename = "type")]
    msg_type: String,
    msg: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct TickerMsg {
    market_ticker: String,
    // Kalshi sends prices as quoted strings e.g. "0.6500"
    yes_bid_dollars: Option<serde_json::Value>,
    yes_ask_dollars: Option<serde_json::Value>,
    last_price_dollars: Option<serde_json::Value>,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run(auth: Arc<KalshiAuth>, pool: Arc<SqlitePool>, client: reqwest::Client) {
    loop {
        if let Err(e) = run_session(&auth, &pool, &client).await {
            error!("WebSocket session ended: {e:#}");
        }
        warn!("Reconnecting in 5s…");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ── Session (runs until disconnect) ──────────────────────────────────────────

async fn run_session(
    auth: &KalshiAuth,
    pool: &SqlitePool,
    client: &reqwest::Client,
) -> Result<()> {
    // 1. Discover markets and upsert to DB.
    let markets = fetch_cs2_markets(client, auth).await?;
    info!("Found {} CS2 markets", markets.len());
    for m in &markets {
        db::upsert_market(pool, m).await?;
    }
    let tickers: Vec<String> = markets.iter().map(|m| m.id.clone()).collect();

    // 2. Connect WebSocket with auth headers.
    let mut request = WS_URL.into_client_request().context("building WS request")?;
    let (key_id, ts, sig) = auth.sign("GET", WS_PATH);
    let headers = request.headers_mut();
    headers.insert("KALSHI-ACCESS-KEY",       key_id.parse()?);
    headers.insert("KALSHI-ACCESS-TIMESTAMP", ts.parse()?);
    headers.insert("KALSHI-ACCESS-SIGNATURE", sig.parse()?);

    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("WebSocket connect failed")?;
    info!("WebSocket connected");

    // 3. Subscribe to ticker channel in batches of 100.
    let mut cmd_id = 1u64;
    for chunk in tickers.chunks(100) {
        let sub = Subscribe {
            id: cmd_id,
            cmd: "subscribe",
            params: SubscribeParams {
                channels: vec!["ticker"],
                market_tickers: chunk.to_vec(),
            },
        };
        ws.send(Message::Text(serde_json::to_string(&sub)?.into()))
            .await
            .context("WS send failed")?;
        cmd_id += 1;
    }
    info!("Subscribed to {} market tickers", tickers.len());

    // 4. Market refresh timer.
    let mut refresh = interval(Duration::from_secs(MARKET_REFRESH_SECS));
    refresh.tick().await; // skip immediate first tick

    // 5. Process messages.
    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_message(&text, pool).await {
                            warn!("Message handling error: {e:#}");
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        ws.send(Message::Pong(p)).await.ok();
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("WS stream ended")),
                }
            }
            _ = refresh.tick() => {
                // Re-fetch markets and subscribe to any new ones.
                match fetch_cs2_markets(client, auth).await {
                    Ok(new_markets) => {
                        let existing: std::collections::HashSet<String> =
                            tickers.iter().cloned().collect();
                        let mut new_tickers = Vec::new();
                        for m in &new_markets {
                            db::upsert_market(pool, m).await.ok();
                            if !existing.contains(&m.id) {
                                new_tickers.push(m.id.clone());
                            }
                        }
                        if !new_tickers.is_empty() {
                            info!("Subscribing to {} new markets", new_tickers.len());
                            for chunk in new_tickers.chunks(100) {
                                let sub = Subscribe {
                                    id: cmd_id,
                                    cmd: "subscribe",
                                    params: SubscribeParams {
                                        channels: vec!["ticker"],
                                        market_tickers: chunk.to_vec(),
                                    },
                                };
                                ws.send(Message::Text(serde_json::to_string(&sub)?.into()))
                                    .await.ok();
                                cmd_id += 1;
                            }
                        }
                    }
                    Err(e) => warn!("Market refresh failed: {e:#}"),
                }
            }
        }
    }
}

fn parse_price(v: Option<serde_json::Value>) -> f64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

// ── Message handler ───────────────────────────────────────────────────────────

async fn handle_message(text: &str, pool: &SqlitePool) -> Result<()> {
    let env: WsEnvelope = serde_json::from_str(text).context("parsing WS envelope")?;

    if env.msg_type != "ticker" {
        return Ok(());
    }

    let msg_val = match env.msg {
        Some(v) => v,
        None => return Ok(()),
    };

    let ticker_msg: TickerMsg =
        serde_json::from_value(msg_val).context("parsing ticker message")?;

    let bid  = parse_price(ticker_msg.yes_bid_dollars);
    let ask  = parse_price(ticker_msg.yes_ask_dollars);
    let last = parse_price(ticker_msg.last_price_dollars);
    let mid  = if ask > 0.0 && bid > 0.0 { (bid + ask) / 2.0 } else { last };

    let tick = PriceTick {
        market_id: ticker_msg.market_ticker.clone(),
        token_id:  ticker_msg.market_ticker.clone(),
        side:      Side::Yes,
        bid,
        ask,
        price: mid,
        timestamp: Utc::now(),
    };

    db::insert_tick(pool, &tick).await?;

    info!(
        "{} mid={:.1}% bid={:.1}% ask={:.1}% spread={:.1}pp",
        ticker_msg.market_ticker,
        mid * 100.0,
        bid * 100.0,
        ask * 100.0,
        (ask - bid) * 100.0,
    );

    Ok(())
}
