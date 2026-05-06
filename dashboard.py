import os
import sqlite3
import time

import pandas as pd
import plotly.graph_objects as go
import streamlit as st

DB_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "trading.db")
REFRESH_SECS = 15

st.set_page_config(page_title="CS2 Markets", layout="wide")


def query(sql, params=()):
    with sqlite3.connect(DB_PATH, timeout=5) as conn:
        conn.execute("PRAGMA journal_mode=WAL")
        return pd.read_sql_query(sql, conn, params=params)


@st.cache_data(ttl=REFRESH_SECS)
def load_markets():
    return query("""
        SELECT m.id, m.question,
               ROUND(t.price * 100, 1)        AS mid_pct,
               ROUND(t.bid   * 100, 1)        AS bid_pct,
               ROUND(t.ask   * 100, 1)        AS ask_pct,
               ROUND((t.ask - t.bid) * 100, 1) AS spread_pp,
               COALESCE(c.tick_count, 0)       AS ticks
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


# ── Layout ────────────────────────────────────────────────────────────────────

markets = load_markets()
with_data = markets[markets["ticks"] > 0]

st.title("CS2 — Kalshi Live Markets")
col_a, col_b, col_c = st.columns(3)
col_a.metric("Total markets", len(markets))
col_b.metric("With live data", len(with_data))
col_c.metric("DB", os.path.basename(DB_PATH))

st.divider()

left, right = st.columns([1, 2])

#  Left: market selector 
with left:
    search = st.text_input("Filter", placeholder="team name, map…")

    filtered = with_data if not search else with_data[
        with_data["question"].str.contains(search, case=False, na=False)
    ]

    if filtered.empty:
        st.info("No markets match filter.")
        selected_id = None
    else:
        options = filtered["id"].tolist()
        labels  = {
            row["id"]: f"{row['question'][:60]}  [{row['mid_pct']}% / {row['spread_pp']}pp / {row['ticks']} ticks]"
            for _, row in filtered.iterrows()
        }
        # Restore previous selection across auto-refreshes
        prev = st.session_state.get("selected_id")
        default_idx = options.index(prev) if prev in options else 0
        selected_id = st.radio(
            "Markets",
            options,
            index=default_idx,
            format_func=lambda x: labels.get(x, x),
            label_visibility="collapsed",
            key="selected_id",
        )

# Right: chart
with right:
    if selected_id:
        row = markets[markets["id"] == selected_id].iloc[0]
        st.subheader(row["question"])

        ticks = load_ticks(selected_id)

        if ticks.empty:
            st.info("No ticks yet.")
        else:
            ticks["time"] = pd.to_datetime(ticks["timestamp"], unit="ms")

            fig = go.Figure()

            # Spread band
            fig.add_trace(go.Scatter(
                x=list(ticks["time"]) + list(ticks["time"][::-1]),
                y=list(ticks["ask"])  + list(ticks["bid"][::-1]),
                fill="toself", fillcolor="rgba(255,255,255,0.06)",
                line=dict(width=0), hoverinfo="skip", showlegend=False,
            ))
            fig.add_trace(go.Scatter(
                x=ticks["time"], y=ticks["ask"], name="Ask",
                line=dict(color="#ef5350", width=1, dash="dot"), mode="lines",
            ))
            fig.add_trace(go.Scatter(
                x=ticks["time"], y=ticks["bid"], name="Bid",
                line=dict(color="#42a5f5", width=1, dash="dot"), mode="lines",
            ))
            fig.add_trace(go.Scatter(
                x=ticks["time"], y=ticks["mid"], name="Mid",
                line=dict(color="#66bb6a", width=2), mode="lines+markers",
                marker=dict(size=3),
            ))

            fig.update_layout(
                template="plotly_dark", margin=dict(l=0, r=0, t=10, b=0),
                yaxis=dict(range=[0, 100], ticksuffix="%"),
                xaxis_title="Time", yaxis_title="Win probability",
                hovermode="x unified", legend=dict(orientation="h", y=1.08),
            )
            st.plotly_chart(fig, use_container_width=True)

            # Stats
            baseline = ticks["mid"].iloc[:5].mean()
            latest   = ticks["mid"].iloc[-1]
            c1, c2, c3, c4 = st.columns(4)
            c1.metric("Mid",      f"{row['mid_pct']}%")
            c2.metric("Spread",   f"{row['spread_pp']}pp")
            c3.metric("Baseline", f"{baseline:.1f}%")
            c4.metric("Δ from BL",f"{latest - baseline:+.1f}pp",
                      delta_color="inverse" if latest < baseline else "normal")
    else:
        st.info("← Select a market to see its chart.")

#  Auto-refresh
st.caption(f"Refreshes every {REFRESH_SECS}s · {time.strftime('%H:%M:%S')} · `{DB_PATH}`")
time.sleep(REFRESH_SECS)
st.cache_data.clear()
st.rerun()
