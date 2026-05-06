//! Backtester — replays stored price ticks and simulates the mean-reversion
//! strategy, reporting PnL, win-rate, and trade log.
//!
//! Strategy logic
//! ──────────────
//! 1. Establish a "baseline" probability from the first N minutes of data.
//! 2. If the YES price drops more than `entry_drop` below baseline AND
//!    we are not within `end_guard_pct` of the market's expected end time,
//!    enter a LONG YES position.
//! 3. Exit when:
//!    a) Price reverts to within `exit_revert_pct` of baseline  (profit)
//!    b) Price drops a further `stop_loss` below entry           (stop-loss)
//!    c) Market is within `end_guard_pct` of its lifetime        (time stop)
//!
//! All thresholds are tunable via CLI flags.

use std::fmt;

use anyhow::Result;
use tracing::info;

use trading::db;

// ── Strategy parameters (tune these for backtesting) ─────────────────────────

#[derive(Debug, Clone)]
struct StrategyConfig {
    /// Drop from baseline that triggers entry (e.g. 0.20 = 20 pp drop).
    entry_drop: f64,
    /// How close to baseline counts as "reverted" for exit (e.g. 0.05 = 5 pp).
    exit_revert_pct: f64,
    /// Stop-loss: additional drop below entry price (e.g. 0.10 = 10 pp further).
    stop_loss: f64,
    /// Fraction of market lifetime to treat as "too close to end" (e.g. 0.15 = last 15%).
    end_guard_pct: f64,
    /// How many ticks to use when computing the opening baseline.
    baseline_ticks: usize,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            entry_drop: 0.20,
            exit_revert_pct: 0.05,
            stop_loss: 0.10,
            end_guard_pct: 0.15,
            baseline_ticks: 5,
        }
    }
}

// ── Trade record ──────────────────────────────────────────────────────────────

#[derive(Debug)]
enum ExitReason {
    Reverted,
    StopLoss,
    TimeStop,
    EndOfData,
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitReason::Reverted => write!(f, "REVERTED"),
            ExitReason::StopLoss => write!(f, "STOP_LOSS"),
            ExitReason::TimeStop => write!(f, "TIME_STOP"),
            ExitReason::EndOfData => write!(f, "END_OF_DATA"),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct Trade {
    entry_price: f64,
    exit_price: f64,
    baseline: f64,
    entry_ts: i64,
    exit_ts: i64,
    exit_reason: ExitReason,
    /// Signed PnL in probability points (exit - entry).
    pnl_pp: f64,
}

// ── Backtesting engine ────────────────────────────────────────────────────────

fn backtest_market(
    // (timestamp_ms, bid, ask, mid)
    ticks: &[(i64, f64, f64, f64)],
    market_start: Option<i64>,
    market_end: Option<i64>,
    cfg: &StrategyConfig,
) -> Vec<Trade> {
    if ticks.len() < cfg.baseline_ticks + 2 {
        return vec![];
    }

    // Baseline = mean mid of first `baseline_ticks` observations.
    let baseline: f64 =
        ticks[..cfg.baseline_ticks].iter().map(|(_, _, _, mid)| mid).sum::<f64>()
            / cfg.baseline_ticks as f64;

    // Market lifetime for the time-guard.
    let t_start = market_start.unwrap_or(ticks[0].0);
    let t_end = market_end.unwrap_or(ticks[ticks.len() - 1].0);
    let lifetime = (t_end - t_start) as f64;
    let guard_start = t_end - (lifetime * cfg.end_guard_pct) as i64;

    let mut trades: Vec<Trade> = vec![];
    let mut in_trade: Option<(f64, i64)> = None; // (entry_ask, entry_ts)

    for &(ts, bid, ask, mid) in &ticks[cfg.baseline_ticks..] {
        match in_trade {
            None => {
                // Signal uses mid to detect the drop from baseline.
                let drop = baseline - mid;
                let near_end = ts >= guard_start;
                if drop >= cfg.entry_drop && !near_end {
                    // We enter by buying at the ask (taker cost).
                    in_trade = Some((ask, ts));
                }
            }
            Some((entry_ask, entry_ts)) => {
                // Exit checks use mid for signal, but we exit at the bid.
                let reverted = (mid - baseline).abs() <= cfg.exit_revert_pct;
                let stopped = entry_ask - bid >= cfg.stop_loss;
                let timed_out = ts >= guard_start;

                let exit_reason = if reverted {
                    Some(ExitReason::Reverted)
                } else if stopped {
                    Some(ExitReason::StopLoss)
                } else if timed_out {
                    Some(ExitReason::TimeStop)
                } else {
                    None
                };

                if let Some(reason) = exit_reason {
                    // We exit by selling at the bid (taker).
                    trades.push(Trade {
                        entry_price: entry_ask,
                        exit_price: bid,
                        baseline,
                        entry_ts,
                        exit_ts: ts,
                        pnl_pp: bid - entry_ask,
                        exit_reason: reason,
                    });
                    in_trade = None;
                }
            }
        }
    }

    // Close any open trade at end of data.
    if let Some((entry_ask, entry_ts)) = in_trade {
        let last = ticks[ticks.len() - 1];
        trades.push(Trade {
            entry_price: entry_ask,
            exit_price: last.1, // bid at last tick
            baseline,
            entry_ts,
            exit_ts: last.0,
            pnl_pp: last.1 - entry_ask,
            exit_reason: ExitReason::EndOfData,
        });
    }

    trades
}

// ── Reporting ─────────────────────────────────────────────────────────────────

fn print_summary(market_question: &str, trades: &[Trade]) {
    if trades.is_empty() {
        println!("  [no trades]");
        return;
    }

    let total_pnl: f64 = trades.iter().map(|t| t.pnl_pp).sum();
    let wins = trades.iter().filter(|t| t.pnl_pp > 0.0).count();
    let win_rate = wins as f64 / trades.len() as f64 * 100.0;

    println!("  Market : {market_question}");
    println!(
        "  Trades : {}  |  Win rate : {:.0}%  |  Total PnL : {:+.1} pp",
        trades.len(),
        win_rate,
        total_pnl * 100.0
    );

    for (i, t) in trades.iter().enumerate() {
        println!(
            "    #{i:02} entry={:.2}% exit={:.2}% baseline={:.2}%  PnL={:+.1}pp  [{}]",
            t.entry_price * 100.0,
            t.exit_price * 100.0,
            t.baseline * 100.0,
            t.pnl_pp * 100.0,
            t.exit_reason,
        );
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let db_path = std::env::var("DB_PATH")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/trading.db").into());
    let pool = db::connect(&db_path).await?;

    let cfg = StrategyConfig::default();
    info!("Strategy config: {cfg:#?}");

    let markets = db::load_all_markets(&pool).await?;
    info!("Backtesting {} markets", markets.len());

    let mut all_trades: Vec<Trade> = vec![];

    for market in &markets {
        let ticks = db::load_yes_ticks(&pool, &market.id).await?;
        if ticks.is_empty() {
            continue;
        }

        let trades = backtest_market(&ticks, market.start_time, market.end_time, &cfg);
        println!("\n──────────────────────────────────────────────────");
        print_summary(&market.question, &trades);
        all_trades.extend(trades);
    }

    println!("\n══════════════════════════════════════════════════");
    println!("OVERALL RESULTS");
    println!("══════════════════════════════════════════════════");

    if all_trades.is_empty() {
        println!("No trades generated — collect more data first.");
    } else {
        let total_pnl: f64 = all_trades.iter().map(|t| t.pnl_pp).sum();
        let wins = all_trades.iter().filter(|t| t.pnl_pp > 0.0).count();
        let win_rate = wins as f64 / all_trades.len() as f64 * 100.0;

        println!("Total trades : {}", all_trades.len());
        println!("Win rate     : {win_rate:.1}%");
        println!("Total PnL    : {:+.1} pp", total_pnl * 100.0);
        println!(
            "Avg PnL/trade: {:+.1} pp",
            total_pnl / all_trades.len() as f64 * 100.0
        );

        // Breakdown by exit reason.
        println!("\nExit reason breakdown:");
        for reason_label in &["REVERTED", "STOP_LOSS", "TIME_STOP", "END_OF_DATA"] {
            let count = all_trades
                .iter()
                .filter(|t| t.exit_reason.to_string() == *reason_label)
                .count();
            let pnl: f64 = all_trades
                .iter()
                .filter(|t| t.exit_reason.to_string() == *reason_label)
                .map(|t| t.pnl_pp)
                .sum();
            if count > 0 {
                println!(
                    "  {reason_label:<12} : {count:>3} trades  PnL {:+.1} pp",
                    pnl * 100.0
                );
            }
        }
    }

    Ok(())
}
