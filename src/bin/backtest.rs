//! Backtester — three strategies:
//!
//! 1. Mean-reversion  (original)
//!    Buy YES when price drops >entry_drop below baseline, exit on revert/stop/time.
//!
//! 2. Cross-market lag  (strategy 4)
//!    When map 1 resolves (price ≥92% or ≤8% for 3+ consecutive ticks), check
//!    whether the match-winner and map-2 markets have already repriced.
//!    If they lag by more than `cross_lag_threshold`, enter in the implied direction
//!    and exit when the lag closes or a stop is hit.
//!
//! 3. Spread fading  (strategy 1)
//!    When the bid/ask spread spikes to >2× its rolling average AND then compresses
//!    back, fade the price move that happened during the spike.

use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use tracing::info;

use trading::db;
use trading::models::Market;

// ── Tick type alias ───────────────────────────────────────────────────────────

/// (timestamp_ms, bid, ask, mid)
type Tick = (i64, f64, f64, f64);

// ═══════════════════════════════════════════════════════════════════════════════
// SHARED CONFIG & TYPES
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct StrategyConfig {
    // ── Mean-reversion ────────────────────────────────────────────────────────
    entry_drop:      f64,   // 0.20 = 20pp drop from baseline triggers entry
    exit_revert_pct: f64,   // 0.05 = within 5pp of baseline counts as reverted
    stop_loss:       f64,   // 0.10 = 10pp further drop below entry → cut
    end_guard_pct:   f64,   // 0.15 = avoid last 15% of market lifetime
    baseline_ticks:  usize, // ticks used to compute opening baseline

    // ── Cross-market lag ──────────────────────────────────────────────────────
    resolution_threshold: f64,   // 0.92 = mid ≥92% or ≤8% counts as resolved
    resolution_confirm:   usize, // 3 = must hold for this many consecutive ticks
    cross_lag_threshold:  f64,   // 0.08 = 8pp lag in related market triggers entry
    cross_stop_loss:      f64,   // 0.10 = stop loss for cross-market trades

    // ── Spread fading ─────────────────────────────────────────────────────────
    spread_window:     usize, // rolling window for average spread (ticks)
    spread_spike_mult: f64,   // 2.0 = spread must be 2× rolling avg to count as spike
    spread_fade_stop:  f64,   // 0.08 = stop loss for spread fade trades
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            entry_drop:           0.20,
            exit_revert_pct:      0.05,
            stop_loss:            0.10,
            end_guard_pct:        0.15,
            baseline_ticks:       5,
            resolution_threshold: 0.92,
            resolution_confirm:   3,
            cross_lag_threshold:  0.08,
            cross_stop_loss:      0.10,
            spread_window:        20,
            spread_spike_mult:    2.0,
            spread_fade_stop:     0.08,
        }
    }
}

#[derive(Debug)]
enum ExitReason {
    Reverted,
    StopLoss,
    TimeStop,
    EndOfData,
    LagClosed,
    SpreadNormal,
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitReason::Reverted     => write!(f, "REVERTED"),
            ExitReason::StopLoss     => write!(f, "STOP_LOSS"),
            ExitReason::TimeStop     => write!(f, "TIME_STOP"),
            ExitReason::EndOfData    => write!(f, "END_OF_DATA"),
            ExitReason::LagClosed    => write!(f, "LAG_CLOSED"),
            ExitReason::SpreadNormal => write!(f, "SPREAD_NORMAL"),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct Trade {
    entry_price: f64,
    exit_price:  f64,
    entry_ts:    i64,
    exit_ts:     i64,
    exit_reason: ExitReason,
    pnl_pp:      f64,
    note:        String, // extra context (e.g. which market triggered the signal)
}

// ═══════════════════════════════════════════════════════════════════════════════
// MARKET ID PARSING
// ═══════════════════════════════════════════════════════════════════════════════

/// Extract the match key from a Kalshi CS2 market ID.
///
/// Examples:
///   KXCS2MAP-26MAY061500R2ISG-1-R2   → "26MAY061500R2ISG"
///   KXCS2GAME-26MAY061500R2ISG-R2    → "26MAY061500R2ISG"
fn parse_match_key(id: &str) -> Option<&str> {
    // Format: PREFIX-MATCHKEY-...
    // PREFIX is either KXCS2MAP or KXCS2GAME
    let after_prefix = id.strip_prefix("KXCS2MAP-")
        .or_else(|| id.strip_prefix("KXCS2GAME-"))?;
    // Match key is the next segment
    after_prefix.split('-').next()
}

/// Extract map number from a MAP market ID (1 or 2), None for GAME markets.
fn parse_map_number(id: &str) -> Option<u8> {
    if !id.starts_with("KXCS2MAP-") {
        return None;
    }
    // KXCS2MAP-MATCHKEY-MAPNUM-TEAM
    let parts: Vec<&str> = id.split('-').collect();
    // parts[0]=KXCS2MAP, parts[1]=MATCHKEY, parts[2]=MAPNUM, parts[3]=TEAM
    if parts.len() >= 3 {
        parts[2].parse().ok()
    } else {
        None
    }
}

fn is_game_market(id: &str) -> bool {
    id.starts_with("KXCS2GAME-")
}

// ═══════════════════════════════════════════════════════════════════════════════
// MATCH GROUPING
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug)]
struct MatchGroup {
    match_key:    String,
    team_a:       String,
    team_b:       String,
    map1_ticks:   Vec<Tick>,
    map1_id:      String,
    map2_ticks:   Vec<Tick>,
    map2_id:      String,
    game_ticks:   Vec<Tick>,
    game_id:      String,
}

/// Group all markets by match key and pick the best-data side for each slot.
async fn build_match_groups(
    pool: &sqlx::SqlitePool,
    markets: &[Market],
) -> Result<Vec<MatchGroup>> {
    // Collect all ticks keyed by market id
    let mut tick_map: HashMap<String, Vec<Tick>> = HashMap::new();
    for m in markets {
        let ticks = db::load_yes_ticks(pool, &m.id).await?;
        if !ticks.is_empty() {
            tick_map.insert(m.id.clone(), ticks);
        }
    }

    // Group markets by match key
    let mut by_match: HashMap<String, Vec<&Market>> = HashMap::new();
    for m in markets {
        if let Some(key) = parse_match_key(&m.id) {
            by_match.entry(key.to_string()).or_default().push(m);
        }
    }

    let mut groups = Vec::new();

    for (match_key, members) in &by_match {
        // Separate into map1 / map2 / game buckets
        let map1_markets: Vec<&&Market> = members.iter()
            .filter(|m| parse_map_number(&m.id) == Some(1))
            .collect();
        let map2_markets: Vec<&&Market> = members.iter()
            .filter(|m| parse_map_number(&m.id) == Some(2))
            .collect();
        let game_markets: Vec<&&Market> = members.iter()
            .filter(|m| is_game_market(&m.id))
            .collect();

        // Need at least map1 + one other to be useful
        if map1_markets.is_empty() || (map2_markets.is_empty() && game_markets.is_empty()) {
            continue;
        }

        // Pick the market with the most ticks in each bucket
        let best = |bucket: &[&&Market]| -> Option<(String, Vec<Tick>)> {
            bucket.iter()
                .filter_map(|m| tick_map.get(&m.id).map(|t| (m.id.clone(), t.clone())))
                .max_by_key(|(_, t)| t.len())
        };

        let Some((map1_id, map1_ticks)) = best(&map1_markets) else { continue };
        let (map2_id, map2_ticks) = best(&map2_markets).unwrap_or_default();
        let (game_id, game_ticks) = best(&game_markets).unwrap_or_default();

        let team_a = members[0].team_a.clone();
        let team_b = members[0].team_b.clone();

        groups.push(MatchGroup {
            match_key: match_key.clone(),
            team_a,
            team_b,
            map1_ticks,
            map1_id,
            map2_ticks,
            map2_id,
            game_ticks,
            game_id,
        });
    }

    // Sort by match key for consistent output
    groups.sort_by(|a, b| a.match_key.cmp(&b.match_key));
    Ok(groups)
}

// ═══════════════════════════════════════════════════════════════════════════════
// STRATEGY 1: MEAN REVERSION (original, unchanged)
// ═══════════════════════════════════════════════════════════════════════════════

fn backtest_mean_reversion(
    ticks: &[Tick],
    market_start: Option<i64>,
    market_end: Option<i64>,
    cfg: &StrategyConfig,
) -> Vec<Trade> {
    if ticks.len() < cfg.baseline_ticks + 2 {
        return vec![];
    }

    let baseline: f64 = ticks[..cfg.baseline_ticks]
        .iter().map(|(_, _, _, mid)| mid).sum::<f64>()
        / cfg.baseline_ticks as f64;

    let t_start = market_start.unwrap_or(ticks[0].0);
    let t_end   = market_end.unwrap_or(ticks[ticks.len() - 1].0);
    let lifetime = (t_end - t_start) as f64;
    let guard_start = t_end - (lifetime * cfg.end_guard_pct) as i64;

    let mut trades = Vec::new();
    let mut in_trade: Option<(f64, i64)> = None;

    for &(ts, bid, ask, mid) in &ticks[cfg.baseline_ticks..] {
        match in_trade {
            None => {
                let drop = baseline - mid;
                if drop >= cfg.entry_drop && ts < guard_start {
                    in_trade = Some((ask, ts));
                }
            }
            Some((entry_ask, entry_ts)) => {
                let reverted  = (mid - baseline).abs() <= cfg.exit_revert_pct;
                let stopped   = entry_ask - bid >= cfg.stop_loss;
                let timed_out = ts >= guard_start;

                let reason = if reverted   { Some(ExitReason::Reverted) }
                        else if stopped    { Some(ExitReason::StopLoss) }
                        else if timed_out  { Some(ExitReason::TimeStop) }
                        else               { None };

                if let Some(r) = reason {
                    trades.push(Trade {
                        entry_price: entry_ask,
                        exit_price:  bid,
                        entry_ts,
                        exit_ts: ts,
                        pnl_pp: bid - entry_ask,
                        exit_reason: r,
                        note: String::new(),
                    });
                    in_trade = None;
                }
            }
        }
    }

    if let Some((entry_ask, entry_ts)) = in_trade {
        let last = ticks[ticks.len() - 1];
        trades.push(Trade {
            entry_price: entry_ask,
            exit_price:  last.1,
            entry_ts,
            exit_ts: last.0,
            pnl_pp: last.1 - entry_ask,
            exit_reason: ExitReason::EndOfData,
            note: String::new(),
        });
    }

    trades
}

// CROSS-MARKET LAG

/// Returns the timestamp and direction of map1 resolution, if any.
/// direction: true = team_a won (price went to 1.0), false = team_b won (price → 0.0)
fn detect_resolution(ticks: &[Tick], cfg: &StrategyConfig) -> Option<(i64, bool)> {
    let hi = cfg.resolution_threshold;
    let lo = 1.0 - hi;
    let confirm = cfg.resolution_confirm;

    let mut streak_hi = 0usize;
    let mut streak_lo = 0usize;

    for &(ts, _, _, mid) in ticks {
        if mid >= hi {
            streak_hi += 1;
            streak_lo = 0;
        } else if mid <= lo {
            streak_lo += 1;
            streak_hi = 0;
        } else {
            streak_hi = 0;
            streak_lo = 0;
        }

        if streak_hi >= confirm {
            return Some((ts, true));
        }
        if streak_lo >= confirm {
            return Some((ts, false));
        }
    }
    None
}

/// Get the mid price of `ticks` at or just after `timestamp`.
fn mid_at(ticks: &[Tick], timestamp: i64) -> Option<f64> {
    ticks.iter()
        .find(|&&(ts, _, _, _)| ts >= timestamp)
        .map(|&(_, _, _, mid)| mid)
}

/// Backtest cross-market lag for one match group.
/// Trades on the match-winner and/or map-2 market when map-1 resolves
/// and those markets haven't repriced yet.
fn backtest_cross_market(group: &MatchGroup, cfg: &StrategyConfig) -> Vec<Trade> {
    let mut trades = Vec::new();

    // Need map1 ticks to detect resolution
    if group.map1_ticks.is_empty() {
        return trades;
    }

    let Some((resolve_ts, team_a_won)) = detect_resolution(&group.map1_ticks, cfg) else {
        return trades;
    };

    // implied_match_prob: after map1 resolves, what should the match winner
    // probability be? In a best-of-3:
    //   - if team_a wins map1: ~0.72 (rough empirical prior, map2 still to play)
    //   - if team_a loses map1: ~0.28
    
    // This is a very very rough estimate in order to start testing the concept of cross-market lag.
    let implied_match_prob = if team_a_won { 0.72 } else { 0.28 };

    // Check match winner market lag
    if !group.game_ticks.is_empty() {
        if let Some(game_mid) = mid_at(&group.game_ticks, resolve_ts) {
            let lag = (implied_match_prob - game_mid).abs();
            if lag >= cfg.cross_lag_threshold {
                // Enter in the direction of the implied probability
                let long = implied_match_prob > game_mid; // buy YES if implied > current

                // Find the ask/bid at resolve_ts for entry
                let entry_tick = group.game_ticks.iter()
                    .find(|&&(ts, _, _, _)| ts >= resolve_ts);

                if let Some(&(entry_ts, bid0, ask0, _)) = entry_tick {
                    let entry_price = if long { ask0 } else { bid0 };

                    // Scan forward for exit: lag closes or stop hit
                    let mut exit_trade: Option<Trade> = None;
                    for &(ts, bid, ask, mid) in group.game_ticks.iter()
                        .filter(|&&(ts, _, _, _)| ts > entry_ts)
                    {
                        let current_lag = (implied_match_prob - mid).abs();
                        let lag_closed = current_lag < cfg.cross_lag_threshold / 2.0;

                        let exit_price = if long { bid } else { ask };
                        let pnl = if long {
                            exit_price - entry_price
                        } else {
                            entry_price - exit_price
                        };

                        let stopped = pnl <= -cfg.cross_stop_loss;

                        if lag_closed || stopped {
                            exit_trade = Some(Trade {
                                entry_price,
                                exit_price,
                                entry_ts,
                                exit_ts: ts,
                                pnl_pp: pnl,
                                exit_reason: if lag_closed {
                                    ExitReason::LagClosed
                                } else {
                                    ExitReason::StopLoss
                                },
                                note: format!(
                                    "map1→game  map1_resolved={} implied={:.2} actual={:.2} lag={:.2}pp",
                                    if team_a_won { &group.team_a } else { &group.team_b },
                                    implied_match_prob,
                                    game_mid,
                                    lag * 100.0,
                                ),
                            });
                            break;
                        }
                    }

                    if exit_trade.is_none() {
                        // Close at end of data
                        if let Some(&(ts, bid, ask, _)) = group.game_ticks.last() {
                            let exit_price = if long { bid } else { ask };
                            exit_trade = Some(Trade {
                                entry_price,
                                exit_price,
                                entry_ts,
                                exit_ts: ts,
                                pnl_pp: if long {
                                    exit_price - entry_price
                                } else {
                                    entry_price - exit_price
                                },
                                exit_reason: ExitReason::EndOfData,
                                note: format!(
                                    "map1→game  map1_resolved={} implied={:.2} actual={:.2}",
                                    if team_a_won { &group.team_a } else { &group.team_b },
                                    implied_match_prob,
                                    game_mid,
                                ),
                            });
                        }
                    }

                    if let Some(t) = exit_trade {
                        trades.push(t);
                    }
                }
            }
        }
    }

    // Check map2 market lag
    // After map1 resolves, map2 should be ~50/50 (it's a fresh map).
    // If it's drifted significantly from 0.50, that's a lag signal.
    if !group.map2_ticks.is_empty() {
        if let Some(map2_mid) = mid_at(&group.map2_ticks, resolve_ts) {
            let implied_map2 = 0.50; // map2 should reset to ~50/50
            let lag = (implied_map2 - map2_mid).abs();

            if lag >= cfg.cross_lag_threshold {
                let long = implied_map2 > map2_mid;

                let entry_tick = group.map2_ticks.iter()
                    .find(|&&(ts, _, _, _)| ts >= resolve_ts);

                if let Some(&(entry_ts, bid0, ask0, _)) = entry_tick {
                    let entry_price = if long { ask0 } else { bid0 };
                    let mut exit_trade: Option<Trade> = None;

                    for &(ts, bid, ask, mid) in group.map2_ticks.iter()
                        .filter(|&&(ts, _, _, _)| ts > entry_ts)
                    {
                        let current_lag = (implied_map2 - mid).abs();
                        let lag_closed = current_lag < cfg.cross_lag_threshold / 2.0;
                        let exit_price = if long { bid } else { ask };
                        let pnl = if long {
                            exit_price - entry_price
                        } else {
                            entry_price - exit_price
                        };
                        let stopped = pnl <= -cfg.cross_stop_loss;

                        if lag_closed || stopped {
                            exit_trade = Some(Trade {
                                entry_price,
                                exit_price,
                                entry_ts,
                                exit_ts: ts,
                                pnl_pp: pnl,
                                exit_reason: if lag_closed {
                                    ExitReason::LagClosed
                                } else {
                                    ExitReason::StopLoss
                                },
                                note: format!(
                                    "map1→map2  map1_resolved={} implied=50% actual={:.2}pp lag={:.2}pp",
                                    if team_a_won { &group.team_a } else { &group.team_b },
                                    map2_mid * 100.0,
                                    lag * 100.0,
                                ),
                            });
                            break;
                        }
                    }

                    if exit_trade.is_none() {
                        if let Some(&(ts, bid, ask, _)) = group.map2_ticks.last() {
                            let exit_price = if long { bid } else { ask };
                            exit_trade = Some(Trade {
                                entry_price,
                                exit_price,
                                entry_ts,
                                exit_ts: ts,
                                pnl_pp: if long {
                                    exit_price - entry_price
                                } else {
                                    entry_price - exit_price
                                },
                                exit_reason: ExitReason::EndOfData,
                                note: format!(
                                    "map1→map2  implied=50% actual={:.2}pp",
                                    map2_mid * 100.0,
                                ),
                            });
                        }
                    }

                    if let Some(t) = exit_trade {
                        trades.push(t);
                    }
                }
            }
        }
    }

    trades
}

// SPREAD FADING

fn backtest_spread_fade(ticks: &[Tick], cfg: &StrategyConfig) -> Vec<Trade> {
    let w = cfg.spread_window;
    if ticks.len() < w + 2 {
        return vec![];
    }

    let mut trades = Vec::new();

    // State machine: Normal → Spiking (track price at spike start) → Fading
    #[derive(Debug, PartialEq)]
    enum State { Normal, Spiking { mid_at_spike: f64, spike_ts: i64 }, InTrade }

    let mut state = State::Normal;
    let mut in_trade: Option<(f64, i64, bool)> = None; // (entry_price, ts, long)

    for i in w..ticks.len() {
        let (ts, bid, ask, mid) = ticks[i];
        let spread = ask - bid;

        // Rolling average spread over window
        let avg_spread: f64 = ticks[i - w..i]
            .iter()
            .map(|&(_, b, a, _)| a - b)
            .sum::<f64>()
            / w as f64;

        let is_spike = avg_spread > 0.0 && spread > cfg.spread_spike_mult * avg_spread;

        match &state {
            State::Normal => {
                if is_spike {
                    state = State::Spiking { mid_at_spike: mid, spike_ts: ts };
                }

                // Also handle open trade exits here
                if let Some((entry_price, _, long)) = in_trade {
                    let exit_price = if long { bid } else { ask };
                    let pnl = if long {
                        exit_price - entry_price
                    } else {
                        entry_price - exit_price
                    };
                    let stopped = pnl <= -cfg.spread_fade_stop;
                    // Exit when spread normalizes (we just entered Normal) or stop hit
                    trades.push(Trade {
                        entry_price,
                        exit_price,
                        entry_ts: in_trade.unwrap().1,
                        exit_ts: ts,
                        pnl_pp: pnl,
                        exit_reason: if stopped {
                            ExitReason::StopLoss
                        } else {
                            ExitReason::SpreadNormal
                        },
                        note: format!("spread_fade  avg={:.3} spike_mult={:.1}x",
                            avg_spread, spread / avg_spread.max(0.001)),
                    });
                    in_trade = None;
                    state = State::Normal;
                }
            }

            State::Spiking { mid_at_spike, spike_ts: _ } => {
                if !is_spike && in_trade.is_none() {
                    // Spike ended — fade the move that happened during it
                    let move_during_spike = mid - mid_at_spike;
                    let long = move_during_spike < 0.0; // price fell → expect revert up → go long
                    let entry_price = if long { ask } else { bid };
                    in_trade = Some((entry_price, ts, long));
                    state = State::InTrade;
                } else if !is_spike {
                    state = State::Normal;
                }
            }

            State::InTrade => {
                if let Some((entry_price, entry_ts, long)) = in_trade {
                    let exit_price = if long { bid } else { ask };
                    let pnl = if long {
                        exit_price - entry_price
                    } else {
                        entry_price - exit_price
                    };
                    let stopped   = pnl <= -cfg.spread_fade_stop;
                    // New spike is another signal to exit
                    let new_spike = is_spike;

                    if stopped || new_spike {
                        trades.push(Trade {
                            entry_price,
                            exit_price,
                            entry_ts,
                            exit_ts: ts,
                            pnl_pp: pnl,
                            exit_reason: if stopped {
                                ExitReason::StopLoss
                            } else {
                                ExitReason::SpreadNormal
                            },
                            note: format!("spread_fade  stopped={stopped} new_spike={new_spike}"),
                        });
                        in_trade = None;
                        state = if new_spike {
                            State::Spiking { mid_at_spike: mid, spike_ts: ts }
                        } else {
                            State::Normal
                        };
                    }
                }
            }
        }
    }

    // Close any open trade at end of data
    if let Some((entry_price, entry_ts, long)) = in_trade {
        let last = ticks[ticks.len() - 1];
        let exit_price = if long { last.1 } else { last.2 };
        trades.push(Trade {
            entry_price,
            exit_price,
            entry_ts,
            exit_ts: last.0,
            pnl_pp: if long {
                exit_price - entry_price
            } else {
                entry_price - exit_price
            },
            exit_reason: ExitReason::EndOfData,
            note: "spread_fade".to_string(),
        });
    }

    trades
}

fn print_trades(label: &str, trades: &[Trade]) {
    if trades.is_empty() {
        println!("  {label}: no trades");
        return;
    }
    let total: f64 = trades.iter().map(|t| t.pnl_pp).sum();
    let wins = trades.iter().filter(|t| t.pnl_pp > 0.0).count();
    let wr = wins as f64 / trades.len() as f64 * 100.0;
    println!(
        "  {label}: {} trades  win={:.0}%  PnL={:+.1}pp",
        trades.len(), wr, total * 100.0
    );
    for (i, t) in trades.iter().enumerate() {
        println!(
            "    #{i:02}  entry={:.1}% exit={:.1}%  pnl={:+.1}pp  [{}]  {}",
            t.entry_price * 100.0,
            t.exit_price  * 100.0,
            t.pnl_pp      * 100.0,
            t.exit_reason,
            t.note,
        );
    }
}

fn print_overall(label: &str, trades: &[Trade]) {
    println!("\n══════════════════════════════════════════════════");
    println!("OVERALL — {label}");
    println!("══════════════════════════════════════════════════");
    if trades.is_empty() {
        println!("  no trades — collect more data first.");
        return;
    }
    let total: f64  = trades.iter().map(|t| t.pnl_pp).sum();
    let wins         = trades.iter().filter(|t| t.pnl_pp > 0.0).count();
    let wr           = wins as f64 / trades.len() as f64 * 100.0;
    let avg          = total / trades.len() as f64;
    println!("  trades    : {}", trades.len());
    println!("  win rate  : {wr:.1}%");
    println!("  total PnL : {:+.1}pp", total * 100.0);
    println!("  avg/trade : {:+.1}pp", avg * 100.0);
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let db_path = std::env::var("DB_PATH")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/trading.db").into());
    let pool = db::connect(&db_path).await?;

    let cfg = StrategyConfig::default();
    info!("Config: {cfg:#?}");

    let markets = db::load_all_markets(&pool).await?;
    info!("Loaded {} markets", markets.len());

    // ── Build match groups (needed for cross-market strategy) ─────────────────
    let groups = build_match_groups(&pool, &markets).await?;
    info!("Built {} match groups", groups.len());

    // ── Strategy 1: Mean reversion ────────────────────────────────────────────
    println!("\n\n╔══════════════════════════════════════════════════╗");
    println!("║  STRATEGY 1 — MEAN REVERSION                    ║");
    println!("╚══════════════════════════════════════════════════╝");

    let mut mr_trades: Vec<Trade> = Vec::new();
    for market in &markets {
        let ticks = db::load_yes_ticks(&pool, &market.id).await?;
        if ticks.is_empty() { continue; }
        let trades = backtest_mean_reversion(&ticks, market.start_time, market.end_time, &cfg);
        if !trades.is_empty() {
            println!("\n  ── {} ──", market.question);
            print_trades("mean-reversion", &trades);
        }
        mr_trades.extend(trades);
    }
    print_overall("MEAN REVERSION", &mr_trades);

    // ── Strategy 2: Cross-market lag ──────────────────────────────────────────
    println!("\n\n╔══════════════════════════════════════════════════╗");
    println!("║  STRATEGY 2 — CROSS-MARKET LAG                  ║");
    println!("╚══════════════════════════════════════════════════╝");

    let mut cm_trades: Vec<Trade> = Vec::new();
    for group in &groups {
        let trades = backtest_cross_market(group, &cfg);
        if !trades.is_empty() {
            println!("\n  ── {} vs {} ──", group.team_a, group.team_b);
            print_trades("cross-market", &trades);
        }
        cm_trades.extend(trades);
    }
    print_overall("CROSS-MARKET LAG", &cm_trades);

    // ── Strategy 3: Spread fading ─────────────────────────────────────────────
    println!("\n\n╔══════════════════════════════════════════════════╗");
    println!("║  STRATEGY 3 — SPREAD FADING                     ║");
    println!("╚══════════════════════════════════════════════════╝");

    let mut sf_trades: Vec<Trade> = Vec::new();
    for market in &markets {
        let ticks = db::load_yes_ticks(&pool, &market.id).await?;
        if ticks.is_empty() { continue; }
        let trades = backtest_spread_fade(&ticks, &cfg);
        if !trades.is_empty() {
            println!("\n  ── {} ──", market.question);
            print_trades("spread-fade", &trades);
        }
        sf_trades.extend(trades);
    }
    print_overall("SPREAD FADING", &sf_trades);

    Ok(())
}