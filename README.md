## requirements 
- rust
- cargo
- python 
- pip 
- kalshi api key: https://docs.kalshi.com/getting_started/api_keys


## setup 
- bash ´´python -m venv venv´´
- bash ´´pip install -r requirements.txt´´
- from /trading ´´cargo build´´ 

### kalshi api key 

1. generate key, https://docs.kalshi.com/getting_started/api_keys
2. put RSA private key under /keys/kalshi.key
3. in .env put your uuid in KALSHI_KEY_ID=

## collector.rs
- fetches all active CS2 markets from Kalshi on startup
- subscribes to live price ticks (bid, ask, mid) via WebSocket
- saves every tick to `trading.db`

## backtest.rs 

- reads from `trading.db`
- computes a baseline probability from the first few ticks of each market
- enters a long YES position when the price drops x pp+ below baseline
- exits when price reverts to baseline, hits a x pp stop-loss, or time runs out

### tunable parameters (in `StrategyConfig`)

| parameter | default | description |
|---|---|---|
| `entry_drop` | 0.20 | drop from baseline to trigger entry |
| `exit_revert_pct` | 0.05 | how close to baseline counts as reverted |
| `stop_loss` | 0.10 | additional drop below entry before cutting loss |
| `end_guard_pct` | 0.15 | fraction of market lifetime to avoid trading near expiry |
| `baseline_ticks` | 5 | ticks used to compute opening baseline |

### Output
per-market trade log with entry/exit prices and pnl, plus an overall summary of win rate, total PnL, and breakdown by exit reason.

## dashboard.py 

- live bid/ask/mid chart per market with spread band
- market list with filter by team name or map
- stats: current mid, spread, baseline probability, and drift from baseline
- auto-refreshes every 15 seconds

## how to run 

### dashboard.py
```bash
streamlit run dashboard.py
```

### backtest.rs 
from root: cargo run --bin backtest 

### collector.rs
from root: cargo run --bin collector


## demo 
trading.db comes populated with data. Launch dashboard to inspect, and backtest to tune parameters. 