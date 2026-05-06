use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use super::auth::KalshiAuth;
use crate::models::Market;

const BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

// We collect both map-winner and match-winner markets.
const CS2_SERIES: &[&str] = &["KXCS2MAP", "KXCS2GAME"];

#[derive(Debug, Deserialize)]
struct KalshiMarket {
    ticker: String,
    event_ticker: String,
    title: String,
    // Kalshi returns prices as quoted decimal strings e.g. "0.9700"
    #[allow(dead_code)]
    yes_bid_dollars: Option<serde_json::Value>,
    #[allow(dead_code)]
    yes_ask_dollars: Option<serde_json::Value>,
    open_time: Option<String>,
    close_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarketsResponse {
    markets: Vec<KalshiMarket>,
    cursor: Option<String>,
}

pub async fn fetch_cs2_markets(client: &Client, auth: &KalshiAuth) -> Result<Vec<Market>> {
    let mut all = Vec::new();
    for &series in CS2_SERIES {
        let mut cursor = String::new();
        loop {
            let path = format!(
                "/markets?series_ticker={}&status=open&limit=200&cursor={}",
                series, cursor
            );
            let (key_id, ts, sig) = auth.sign("GET", &path);

            let resp: MarketsResponse = client
                .get(format!("{}{}", BASE, path))
                .header("KALSHI-ACCESS-KEY", &key_id)
                .header("KALSHI-ACCESS-TIMESTAMP", &ts)
                .header("KALSHI-ACCESS-SIGNATURE", &sig)
                .header("Accept", "application/json")
                .send()
                .await
                .context("Kalshi REST request failed")?
                .json()
                .await
                .context("Kalshi REST JSON parse failed")?;

            let page_len = resp.markets.len();
            for m in resp.markets {
                all.push(to_market(m));
            }

            match resp.cursor {
                Some(c) if !c.is_empty() && page_len == 200 => cursor = c,
                _ => break,
            }
        }
    }
    Ok(all)
}

fn to_market(m: KalshiMarket) -> Market {
    let (team_a, team_b) = extract_teams(&m.title);
    Market {
        id: m.ticker.clone(),
        question: m.title,
        team_a,
        team_b,
        token_yes: m.ticker.clone(), // Kalshi uses ticker as the identifier
        token_no: m.event_ticker,
        start_time: m.open_time.as_deref().and_then(parse_ts),
        end_time: m.close_time.as_deref().and_then(parse_ts),
        active: true,
    }
}

/// "Will NaVi win map 2 in the NaVi vs. Vitality match?" → ("NaVi", "Vitality")
fn extract_teams(title: &str) -> (String, String) {
    // Pattern: "Will X win ... in the A vs. B match?"
    if let Some(vs_part) = title.split(" in the ").nth(1) {
        let matchup = vs_part.trim_end_matches(" match?").trim_end_matches('?');
        if let Some((a, b)) = matchup.split_once(" vs. ") {
            return (a.trim().to_string(), b.trim().to_string());
        }
    }
    // Pattern: "Will X win the A vs. B CS2 match?"
    if let Some(vs_part) = title.split(" the ").nth(1) {
        let clean = vs_part
            .trim_end_matches(" CS2 match?")
            .trim_end_matches(" match?");
        if let Some((a, b)) = clean.split_once(" vs. ") {
            return (a.trim().to_string(), b.trim().to_string());
        }
    }
    (title.to_string(), String::new())
}

fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}
