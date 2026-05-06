use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A CS2 map-winner market stored in our DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub id: String,
    pub question: String,
    pub team_a: String,
    pub team_b: String,
    pub token_yes: String,
    pub token_no: String,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub active: bool,
}

/// A single price observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceTick {
    pub market_id: String,
    pub token_id: String,
    pub side: Side,
    /// Best bid — what you receive when selling (taker).
    pub bid: f64,
    /// Best ask — what you pay when buying (taker).
    pub ask: f64,
    /// Mid-price = (bid + ask) / 2  — implied probability.
    pub price: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Yes => write!(f, "yes"),
            Side::No => write!(f, "no"),
        }
    }
}
