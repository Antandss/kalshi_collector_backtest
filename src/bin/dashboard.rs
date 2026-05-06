//! Live dashboard — run alongside the collector.
//! Opens a local web server at http://localhost:3000
//!
//! Usage:
//!   cargo run --bin dashboard

use std::sync::Arc;

use anyhow::Result;
use axum::{
    Router,
    extract::{Path, State},
    response::{Html, Json},
    routing::get,
};
use serde::Serialize;
use sqlx::SqlitePool;

use trading::db;

type AppState = Arc<SqlitePool>;

#[tokio::main]
async fn main() -> Result<()> {
    let db_path = std::env::var("DB_PATH")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/trading.db").into());
    let pool = Arc::new(db::connect(&db_path).await?);

    let app = Router::new()
        .route("/", get(index_html))
        .route("/api/markets", get(api_markets))
        .route("/api/prices/{market_id}", get(api_prices))
        .with_state(pool);

    let addr = "127.0.0.1:3000";
    println!("Dashboard running at http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ── API handlers ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MarketSummary {
    id: String,
    question: String,
    team_a: String,
    team_b: String,
    latest_mid: Option<f64>,
    latest_bid: Option<f64>,
    latest_ask: Option<f64>,
    spread_pp: Option<f64>,
    tick_count: i64,
}

async fn api_markets(State(pool): State<AppState>) -> Json<Vec<MarketSummary>> {
    let markets = db::load_all_markets(&pool).await.unwrap_or_default();
    let mut summaries = Vec::new();

    use sqlx::Row;
    for m in markets {
        // Two separate queries avoids mixing aggregates with non-aggregates.
        let cnt: i64 = sqlx::query(
            "SELECT COUNT(*) as cnt FROM price_ticks WHERE market_id = ? AND side = 'yes'",
        )
        .bind(&m.id)
        .fetch_one(pool.as_ref())
        .await
        .map(|r| r.try_get("cnt").unwrap_or(0))
        .unwrap_or(0);

        let latest = sqlx::query(
            "SELECT bid, ask, price FROM price_ticks
             WHERE market_id = ? AND side = 'yes'
             ORDER BY timestamp DESC LIMIT 1",
        )
        .bind(&m.id)
        .fetch_optional(pool.as_ref())
        .await
        .unwrap_or(None);

        let (latest_mid, latest_bid, latest_ask, spread_pp) = match latest {
            Some(r) => {
                let bid: Option<f64> = r.try_get("bid").ok().flatten();
                let ask: Option<f64> = r.try_get("ask").ok().flatten();
                let mid: f64 = r.try_get("price").unwrap_or(0.0);
                let spread = bid.zip(ask).map(|(b, a)| (a - b) * 100.0);
                (Some(mid), bid, ask, spread)
            }
            None => (None, None, None, None),
        };

        let tick_count = cnt;

        summaries.push(MarketSummary {
            id: m.id,
            question: m.question,
            team_a: m.team_a,
            team_b: m.team_b,
            latest_mid,
            latest_bid,
            latest_ask,
            spread_pp,
            tick_count,
        });
    }

    // Sort: markets with data first, then by question
    summaries.sort_by(|a, b| {
        b.tick_count.cmp(&a.tick_count).then(a.question.cmp(&b.question))
    });

    Json(summaries)
}

#[derive(Serialize)]
struct PricePoint {
    ts: i64,
    bid: f64,
    ask: f64,
    mid: f64,
}

async fn api_prices(
    State(pool): State<AppState>,
    Path(market_id): Path<String>,
) -> Json<Vec<PricePoint>> {
    let rows = sqlx::query(
        "SELECT timestamp, bid, ask, price FROM price_ticks
         WHERE market_id = ? AND side = 'yes'
         ORDER BY timestamp ASC",
    )
    .bind(&market_id)
    .fetch_all(pool.as_ref())
    .await
    .unwrap_or_default();

    use sqlx::Row;
    let points = rows
        .iter()
        .map(|r| {
            let mid: f64 = r.try_get("price").unwrap_or(0.0);
            let bid: f64 = r.try_get::<Option<f64>, _>("bid").ok().flatten().unwrap_or(mid);
            let ask: f64 = r.try_get::<Option<f64>, _>("ask").ok().flatten().unwrap_or(mid);
            PricePoint {
                ts: r.try_get("timestamp").unwrap_or(0),
                bid,
                ask,
                mid,
            }
        })
        .collect();

    Json(points)
}

// ── Inline HTML ───────────────────────────────────────────────────────────────

async fn index_html() -> Html<&'static str> {
    Html(HTML)
}

const HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>CS2 Trading Dashboard</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4"></script>
<script src="https://cdn.jsdelivr.net/npm/chartjs-adapter-date-fns@3"></script>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: monospace; background: #0d0d0d; color: #e0e0e0; }
  h1 { padding: 16px 20px; font-size: 14px; color: #aaa; border-bottom: 1px solid #222; }
  #layout { display: flex; height: calc(100vh - 45px); }
  #sidebar { width: 340px; overflow-y: auto; border-right: 1px solid #222; }
  .market-row {
    padding: 10px 14px; cursor: pointer; border-bottom: 1px solid #1a1a1a;
    transition: background 0.1s;
  }
  .market-row:hover { background: #1a1a1a; }
  .market-row.active { background: #1e2a1e; border-left: 3px solid #4caf50; }
  .market-q { font-size: 11px; color: #ccc; margin-bottom: 4px; }
  .market-meta { font-size: 11px; color: #666; }
  .mid { color: #4caf50; font-weight: bold; }
  .spread { color: #f0a500; }
  .no-data { color: #444; font-style: italic; }
  #main { flex: 1; padding: 20px; display: flex; flex-direction: column; gap: 16px; }
  #chart-title { font-size: 13px; color: #aaa; }
  #chart-wrap { flex: 1; position: relative; }
  #refresh-info { font-size: 11px; color: #444; text-align: right; }
</style>
</head>
<body>
<h1>CS2 Prediction Market Monitor — <span id="market-count">loading…</span></h1>
<div id="layout">
  <div id="sidebar"></div>
  <div id="main">
    <div id="chart-title">Select a market from the sidebar</div>
    <div id="chart-wrap"><canvas id="chart"></canvas></div>
    <div id="refresh-info">Auto-refreshes every 15s</div>
  </div>
</div>

<script>
let chart = null;
let selectedId = null;
let refreshTimer = null;

async function loadMarkets() {
  const res = await fetch('/api/markets');
  const markets = await res.json();
  document.getElementById('market-count').textContent =
    `${markets.length} markets  |  ${markets.filter(m => m.tick_count > 0).length} with data`;

  const sb = document.getElementById('sidebar');
  sb.innerHTML = '';
  for (const m of markets) {
    const div = document.createElement('div');
    div.className = 'market-row' + (m.id === selectedId ? ' active' : '');
    div.dataset.id = m.id;

    const hasData = m.latest_mid !== null;
    const midStr  = hasData ? `<span class="mid">${(m.latest_mid*100).toFixed(1)}%</span>` : '';
    const spreadStr = m.spread_pp !== null
      ? `<span class="spread"> spread ${m.spread_pp.toFixed(1)}pp</span>` : '';
    const tickStr = m.tick_count > 0 ? ` · ${m.tick_count} ticks` : '';

    div.innerHTML = `
      <div class="market-q">${m.question}</div>
      <div class="market-meta">
        ${hasData ? midStr + spreadStr + tickStr : '<span class="no-data">no data yet</span>'}
      </div>`;

    div.addEventListener('click', () => selectMarket(m.id, m.question));
    sb.appendChild(div);
  }
}

async function selectMarket(id, question) {
  selectedId = id;
  document.getElementById('chart-title').textContent = question;
  document.querySelectorAll('.market-row').forEach(el => {
    el.classList.toggle('active', el.dataset.id === id);
  });
  await loadChart(id);
}

async function loadChart(id) {
  const res = await fetch(`/api/prices/${id}`);
  const pts = await res.json();

  if (pts.length === 0) {
    document.getElementById('chart-title').textContent += '  (no ticks yet)';
    return;
  }

  const labels = pts.map(p => new Date(p.ts));
  const mids   = pts.map(p => +(p.mid  * 100).toFixed(2));
  const bids   = pts.map(p => +(p.bid  * 100).toFixed(2));
  const asks   = pts.map(p => +(p.ask  * 100).toFixed(2));

  const ctx = document.getElementById('chart').getContext('2d');
  if (chart) chart.destroy();

  chart = new Chart(ctx, {
    type: 'line',
    data: {
      labels,
      datasets: [
        {
          label: 'Mid (implied prob)',
          data: mids, borderColor: '#4caf50', backgroundColor: 'rgba(76,175,80,0.08)',
          borderWidth: 2, pointRadius: 2, tension: 0.2, fill: false,
        },
        {
          label: 'Bid (you sell at)',
          data: bids, borderColor: '#2196f3', backgroundColor: 'transparent',
          borderWidth: 1, pointRadius: 0, borderDash: [4,3], tension: 0.2,
        },
        {
          label: 'Ask (you buy at)',
          data: asks, borderColor: '#f44336', backgroundColor: 'transparent',
          borderWidth: 1, pointRadius: 0, borderDash: [4,3], tension: 0.2,
        },
      ],
    },
    options: {
      animation: false,
      responsive: true,
      maintainAspectRatio: false,
      interaction: { mode: 'index', intersect: false },
      scales: {
        x: {
          type: 'time',
          time: { tooltipFormat: 'HH:mm:ss', displayFormats: { minute: 'HH:mm', second: 'HH:mm:ss' } },
          ticks: { color: '#555', maxRotation: 0 },
          grid: { color: '#1a1a1a' },
        },
        y: {
          min: 0, max: 100,
          ticks: { color: '#555', callback: v => v + '%' },
          grid: { color: '#1a1a1a' },
          title: { display: true, text: 'Win Probability (%)', color: '#555' },
        },
      },
      plugins: {
        legend: { labels: { color: '#888', font: { family: 'monospace', size: 11 } } },
        tooltip: {
          backgroundColor: '#1a1a1a', titleColor: '#aaa', bodyColor: '#ccc',
          callbacks: { label: ctx => ` ${ctx.dataset.label}: ${ctx.parsed.y.toFixed(1)}%` }
        },
      },
    },
  });
}

async function refresh() {
  await loadMarkets();
  if (selectedId) await loadChart(selectedId);
  document.getElementById('refresh-info').textContent =
    `Last refresh: ${new Date().toLocaleTimeString()}  · auto-refresh 15s`;
}

refresh();
setInterval(refresh, 15000);
</script>
</body>
</html>
"#;
