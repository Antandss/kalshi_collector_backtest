import os
import sqlite3
import time

import pandas as pd
import plotly.graph_objects as go
import streamlit as st

DB_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "trading.db")
REFRESH_SECS = 15

st.set_page_config(page_title="kalshi cs2", layout="wide", page_icon="📈")

# ── Custom styling ─────────────────────────────────────────────────────────────

st.markdown("""
<style>
    @import url('https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;600&family=IBM+Plex+Sans:wght@300;400;600&display=swap');

    html, body, [class*="css"] { font-family: 'IBM Plex Sans', sans-serif; }
    code, .stMetric label { font-family: 'IBM Plex Mono', monospace; }

    .stApp { background: #0d0f12; color: #c9d1d9; }

    .stMetric { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 12px 16px; }
    .stMetric label { color: #8b949e !important; font-size: 11px !important; letter-spacing: 0.08em; text-transform: uppercase; }
    .stMetric [data-testid="metric-container"] > div:nth-child(2) { font-family: 'IBM Plex Mono', monospace; font-size: 22px; color: #e6edf3; }

    div[data-testid="stTabs"] button { font-family: 'IBM Plex Mono', monospace; font-size: 13px; letter-spacing: 0.05em; color: #8b949e; }
    div[data-testid="stTabs"] button[aria-selected="true"] { color: #58a6ff; border-bottom-color: #58a6ff; }

    .stTextInput input { background: #161b22; border: 1px solid #30363d; color: #e6edf3; font-family: 'IBM Plex Mono', monospace; font-size: 13px; }

    h1 { font-family: 'IBM Plex Mono', monospace !important; font-weight: 600 !important; color: #e6edf3 !important; letter-spacing: -0.02em; }
    h3 { font-family: 'IBM Plex Sans', sans-serif !important; font-weight: 300 !important; color: #8b949e !important; font-size: 14px !important; letter-spacing: 0.05em; text-transform: uppercase; }

    .stDivider { border-color: #21262d; }
    .stCaption { color: #484f58 !important; font-family: 'IBM Plex Mono', monospace !important; font-size: 11px !important; }

    .trade-legend { display: flex; gap: 20px; margin: 8px 0; }
    .trade-legend span { font-family: 'IBM Plex Mono', monospace; font-size: 12px; color: #8b949e; }
    .dot-entry { color: #f0883e; }
    .dot-exit-win { color: #3fb950; }
    .dot-exit-loss { color: #f85149; }
</style>
""", unsafe_allow_html=True)

def query(sql, params=()):
    with sqlite3.connect(DB_PATH, timeout=5) as conn:
        conn.execute("PRAGMA journal_mode=WAL")
        return pd.read_sql_query(sql, conn, params=params)


@st.cache_data(ttl=REFRESH_SECS)
def load_markets():
    return query("""
        SELECT m.id, m.question, m.team_a, m.team_b,
               ROUND(t.price * 100, 1)         AS mid_pct,
               ROUND(t.bid   * 100, 1)         AS bid_pct,
               ROUND(t.ask   * 100, 1)         AS ask_pct,
               ROUND((t.ask - t.bid) * 100, 1) AS spread_pp,
               COALESCE(c.tick_count, 0)        AS ticks,
               m.start_time,
               m.end_time
        FROM markets m
        LEFT JOIN (
            SELECT market_id, price, bid, ask
            FROM price_ticks
            WHERE id IN (
                SELECT MAX(id) FROM price_ticks
                WHERE side = 'yes' GROUP BY market_id
            )
        ) t ON t.market_id = m.id
        LEFT JOIN (
            SELECT market_id, COUNT(*) AS tick_count
            FROM price_ticks WHERE side = 'yes'
            GROUP BY market_id
        ) c ON c.market_id = m.id
        ORDER BY ticks DESC, m.id
    """)


@st.cache_data(ttl=REFRESH_SECS)
def load_ticks(market_id):
    return query("""
        SELECT timestamp, bid*100 AS bid, ask*100 AS ask, price*100 AS mid
        FROM price_ticks
        WHERE market_id = ? AND side = 'yes'
        ORDER BY timestamp ASC
    """, params=(market_id,))


# ── Strategy ──────────────────────────────────────────────────────────────────

def run_strategy(ticks_df, start_time, end_time, cfg):
    rows = list(ticks_df.itertuples(index=False))
    n = len(rows)
    bt = cfg["baseline_ticks"]
    if n < bt + 2:
        return []

    baseline = sum(r.mid for r in rows[:bt]) / bt

    t_start = start_time if start_time else rows[0].timestamp
    t_end   = end_time   if end_time   else rows[-1].timestamp
    lifetime = t_end - t_start if t_end != t_start else 1
    guard_start = t_end - lifetime * cfg["end_guard_pct"]

    trades = []
    in_trade = None

    for r in rows[bt:]:
        ts, bid, ask, mid = r.timestamp, r.bid, r.ask, r.mid
        if in_trade is None:
            drop = baseline - mid
            if drop >= cfg["entry_drop"] and ts < guard_start:
                in_trade = (ask, ts)
        else:
            entry_ask, entry_ts = in_trade
            reverted  = abs(mid - baseline) <= cfg["exit_revert_pct"]
            stopped   = entry_ask - bid >= cfg["stop_loss"]
            timed_out = ts >= guard_start
            reason = ("reverted" if reverted else
                      "stop_loss" if stopped else
                      "time_stop" if timed_out else None)
            if reason:
                trades.append({
                    "entry_ts": entry_ts, "entry_price": entry_ask,
                    "exit_ts": ts,        "exit_price": bid,
                    "baseline": baseline, "pnl_pp": bid - entry_ask,
                    "exit_reason": reason,
                })
                in_trade = None

    if in_trade:
        entry_ask, entry_ts = in_trade
        last = rows[-1]
        trades.append({
            "entry_ts": entry_ts, "entry_price": entry_ask,
            "exit_ts": last.timestamp, "exit_price": last.bid,
            "baseline": baseline, "pnl_pp": last.bid - entry_ask,
            "exit_reason": "end_of_data",
        })

    return trades


DARK = "#0d0f12"
GRID = "#21262d"
TEXT = "#8b949e"


def base_chart(ticks):
    ticks = ticks.copy()
    ticks["time"] = pd.to_datetime(ticks["timestamp"], unit="ms")
    fig = go.Figure()
    fig.add_trace(go.Scatter(
        x=list(ticks["time"]) + list(ticks["time"][::-1]),
        y=list(ticks["ask"])  + list(ticks["bid"][::-1]),
        fill="toself", fillcolor="rgba(88,166,255,0.05)",
        line=dict(width=0), hoverinfo="skip", showlegend=False,
    ))
    fig.add_trace(go.Scatter(
        x=ticks["time"], y=ticks["ask"], name="ask",
        line=dict(color="#f85149", width=1, dash="dot"), mode="lines",
    ))
    fig.add_trace(go.Scatter(
        x=ticks["time"], y=ticks["bid"], name="bid",
        line=dict(color="#58a6ff", width=1, dash="dot"), mode="lines",
    ))
    fig.add_trace(go.Scatter(
        x=ticks["time"], y=ticks["mid"], name="mid",
        line=dict(color="#3fb950", width=2), mode="lines+markers",
        marker=dict(size=3),
    ))
    fig.update_layout(
        paper_bgcolor=DARK, plot_bgcolor=DARK,
        margin=dict(l=0, r=0, t=10, b=0),
        font=dict(family="IBM Plex Mono", color=TEXT, size=11),
        yaxis=dict(range=[0, 100], ticksuffix="%", gridcolor=GRID, zerolinecolor=GRID),
        xaxis=dict(gridcolor=GRID, zerolinecolor=GRID),
        xaxis_title=None, yaxis_title="win probability",
        hovermode="x unified",
        legend=dict(orientation="h", y=1.08, font=dict(size=11)),
    )
    return fig, ticks


def add_trade_markers(fig, ticks_with_time, trades):
    if not trades:
        return fig
    ts_map = dict(zip(ticks_with_time["timestamp"], ticks_with_time["time"]))

    bl = trades[0]["baseline"]
    fig.add_hline(
        y=bl, line_dash="dash",
        line_color="rgba(240,136,62,0.4)", line_width=1,
        annotation_text=f"baseline {bl:.1f}%",
        annotation_font=dict(size=10, color="#f0883e"),
        annotation_position="top right",
    )

    for t in trades:
        entry_time = ts_map.get(t["entry_ts"])
        exit_time  = ts_map.get(t["exit_ts"])
        win = t["pnl_pp"] >= 0
        exit_color = "#3fb950" if win else "#f85149"

        if entry_time is not None:
            fig.add_trace(go.Scatter(
                x=[entry_time], y=[t["entry_price"]],
                mode="markers",
                marker=dict(symbol="triangle-up", size=14, color="#f0883e",
                            line=dict(color="#0d0f12", width=1)),
                name="entry", showlegend=False,
                hovertemplate=f"entry @ {t['entry_price']:.1f}%<extra></extra>",
            ))
        if exit_time is not None:
            fig.add_trace(go.Scatter(
                x=[exit_time], y=[t["exit_price"]],
                mode="markers",
                marker=dict(symbol="triangle-down", size=14, color=exit_color,
                            line=dict(color="#0d0f12", width=1)),
                name="exit", showlegend=False,
                hovertemplate=f"exit @ {t['exit_price']:.1f}% [{t['exit_reason']}]<extra></extra>",
            ))
        if entry_time is not None and exit_time is not None:
            fig.add_vrect(
                x0=entry_time, x1=exit_time,
                fillcolor=exit_color, opacity=0.06,
                layer="below", line_width=0,
            )
    return fig


markets = load_markets()
with_data = markets[markets["ticks"] > 0].copy()

st.title("kalshi / cs2")

col_a, col_b, col_c = st.columns(3)
col_a.metric("total markets", len(markets))
col_b.metric("with live data", len(with_data))
col_c.metric("last refresh", time.strftime("%H:%M:%S"))

st.divider()

left, right = st.columns([1, 2])

with left:
    search = st.text_input("filter", placeholder="team name, map…")
    filtered = with_data if not search else with_data[
        with_data["question"].str.contains(search, case=False, na=False)
    ]

    selected_id = st.session_state.get("selected_id")

    if filtered.empty:
        st.info("no markets match.")
        selected_id = None
    else:
        filtered = filtered.copy()
        filtered["match"] = filtered.apply(
            lambda r: f"{r['team_a']} vs {r['team_b']}"
                      if pd.notna(r["team_a"]) and r["team_a"] else "other",
            axis=1,
        )

        # Order matches by total ticks descending
        match_order = (
            filtered.groupby("match")["ticks"].sum()
            .sort_values(ascending=False).index.tolist()
        )

        for match in match_order:
            group = filtered[filtered["match"] == match]
            n = len(group)
            with st.expander(f"**{match}**  ·  {n} market{'s' if n > 1 else ''}", expanded=False):
                for _, mrow in group.iterrows():
                    short = mrow["question"]
                    for strip in [
                        f" in the {match} match?",
                        f" in the {match} CS2 match?",
                        f" the {match} CS2 match?",
                        "?",
                    ]:
                        short = short.replace(strip, "")
                    short = short.replace("Will ", "").strip()
                    label = f"{short[:40]}  [{mrow['mid_pct']}% · {mrow['ticks']}t]"

                    is_active = mrow["id"] == selected_id
                    if st.button(
                        label,
                        key=f"btn_{mrow['id']}",
                        type="primary" if is_active else "secondary",
                        use_container_width=True,
                    ):
                        st.session_state["selected_id"] = mrow["id"]
                        st.rerun()

        selected_id = st.session_state.get("selected_id")

# ── Right: chart panel ────────────────────────────────────────────────────────

with right:
    if not selected_id:
        st.info("← select a market")
    else:
        mdata = markets[markets["id"] == selected_id]
        if mdata.empty:
            st.info("market not found.")
        else:
            mdata = mdata.iloc[0]
            st.subheader(mdata["question"])

            ticks = load_ticks(selected_id)
            if ticks.empty:
                st.info("no ticks yet.")
            else:
                tab_live, tab_backtest = st.tabs(["live", "backtest"])

                with tab_live:
                    fig, ticks_t = base_chart(ticks)
                    st.plotly_chart(fig, use_container_width=True, key="live_chart")

                    baseline = ticks["mid"].iloc[:5].mean()
                    latest   = ticks["mid"].iloc[-1]
                    c1, c2, c3, c4 = st.columns(4)
                    c1.metric("mid",        f"{mdata['mid_pct']}%")
                    c2.metric("spread",     f"{mdata['spread_pp']}pp")
                    c3.metric("baseline",   f"{baseline:.1f}%")
                    c4.metric("Δ baseline", f"{latest - baseline:+.1f}pp",
                              delta_color="inverse" if latest < baseline else "normal")

                with tab_backtest:
                    with st.expander("strategy parameters", expanded=False):
                        col1, col2, col3 = st.columns(3)
                        with col1:
                            entry_drop  = st.slider("entry drop (pp)",  5, 40, 20)
                            exit_revert = st.slider("exit revert (pp)", 1, 15,  5)
                        with col2:
                            stop_loss   = st.slider("stop loss (pp)",   3, 25, 10)
                            end_guard   = st.slider("end guard %",      5, 30, 15)
                        with col3:
                            baseline_n  = st.slider("baseline ticks",   2, 20,  5)

                    cfg = {
                        "entry_drop":      entry_drop,
                        "exit_revert_pct": exit_revert,
                        "stop_loss":       stop_loss,
                        "end_guard_pct":   end_guard / 100,
                        "baseline_ticks":  baseline_n,
                    }

                    trades = run_strategy(ticks, mdata["start_time"], mdata["end_time"], cfg)

                    fig, ticks_t = base_chart(ticks)
                    fig = add_trade_markers(fig, ticks_t, trades)
                    st.plotly_chart(fig, use_container_width=True, key="backtest_chart")

                    st.markdown(
                        '<div class="trade-legend">'
                        '<span class="dot-entry">▲ entry</span>'
                        '<span class="dot-exit-win">▼ exit (profit)</span>'
                        '<span class="dot-exit-loss">▼ exit (loss)</span>'
                        '</div>',
                        unsafe_allow_html=True,
                    )

                    if not trades:
                        st.info("no trades triggered with these parameters.")
                    else:
                        total_pnl = sum(t["pnl_pp"] for t in trades)
                        wins      = sum(1 for t in trades if t["pnl_pp"] >= 0)
                        c1, c2, c3 = st.columns(3)
                        c1.metric("trades",    len(trades))
                        c2.metric("win rate",  f"{wins / len(trades) * 100:.0f}%")
                        c3.metric("total pnl", f"{total_pnl:+.1f}pp",
                                  delta_color="normal" if total_pnl >= 0 else "inverse")

                        df = pd.DataFrame([{
                            "entry time": pd.to_datetime(t["entry_ts"], unit="ms").strftime("%H:%M:%S"),
                            "exit time":  pd.to_datetime(t["exit_ts"],  unit="ms").strftime("%H:%M:%S"),
                            "entry":      f"{t['entry_price']:.1f}%",
                            "exit":       f"{t['exit_price']:.1f}%",
                            "pnl":        f"{t['pnl_pp']:+.1f}pp",
                            "reason":     t["exit_reason"],
                        } for t in trades])
                        st.dataframe(df, use_container_width=True, hide_index=True)

st.caption(f"auto-refresh every {REFRESH_SECS}s  ·  {DB_PATH}")
time.sleep(REFRESH_SECS)
st.cache_data.clear()
st.rerun()