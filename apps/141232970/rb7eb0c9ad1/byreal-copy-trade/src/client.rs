//! Client layer for byreal-copy-trade — byreal's Copy-Farming (`copyfarmer`)
//! discovery endpoints on `api2.byreal.io`.
//!
//! Thin by design: the envelope decoding is byreal-lp's (`byreal_get` /
//! `byreal_post`, all three inner-status variants), and position/pool reads +
//! the open execute come from byreal-lp too. This file owns only the three
//! `copyfarmer` endpoints the byreal-lp app doesn't carry.
//!
//! Endpoint quirks verified live:
//! - `top-positions` is a **POST**; `sortField` is an enum — one of
//!   `liquidity | apr | earned | pnl | bonus | copies | openTime | closeTime |
//!   age` (byreal rejects anything else by name).
//! - `providerOverview` is a GET keyed by `providerAddress` (NOT
//!   `walletAddress`).
//!
//! (`copyfarmer/epoch-bonus` lives in the `byreal` app as
//! `byreal_lp_get_epoch_bonus` — not duplicated here.)

use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;

use byreal_lp::client::{BYREAL_API_BASE, byreal_get, byreal_post};

#[derive(Clone, Default)]
pub struct ByrealCopyTradeApp;

const PATH_TOP_POSITIONS: &str = "/byreal/api/dex/v2/copyfarmer/top-positions";
const PATH_PROVIDER_OVERVIEW: &str = "/byreal/api/dex/v2/copyfarmer/providerOverview";

pub(crate) struct CopyClient {
    http: reqwest::blocking::Client,
    base_url: String,
}

static COPY_CLIENT: OnceLock<Result<CopyClient, String>> = OnceLock::new();

pub(crate) fn copy_client() -> Result<&'static CopyClient, String> {
    COPY_CLIENT
        .get_or_init(CopyClient::new)
        .as_ref()
        .map_err(|e| e.clone())
}

impl CopyClient {
    fn new() -> Result<Self, String> {
        Ok(Self {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| format!("[byreal-copy-trade] failed to build HTTP client: {e}"))?,
            base_url: std::env::var("BYREAL_API_URL")
                .unwrap_or_else(|_| BYREAL_API_BASE.to_string()),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// `/copyfarmer/top-positions` — the Copy-Farming leaderboard. Each record
    /// is a live LP *position* (pool + tick range + NFT + earned/pnl), so a
    /// record is directly mirrorable.
    pub(crate) fn list_top_positions(
        &self,
        page: u32,
        page_size: u32,
        sort_field: &str,
        sort_type: &str,
        pool_address: Option<&str>,
    ) -> Result<Value, String> {
        let mut body = json!({
            "page": page,
            "pageSize": page_size,
            "sortField": sort_field,
            "sortType": sort_type,
            "status": 0,
        });
        if let Some(addr) = pool_address {
            body["poolAddress"] = json!(addr);
        }
        byreal_post(&self.http, &self.url(PATH_TOP_POSITIONS), &body)
    }

    /// `/copyfarmer/providerOverview` — one LP's aggregate stats (follower
    /// count, cumulative earned/pnl) keyed by `providerAddress`.
    pub(crate) fn provider_overview(&self, provider_address: &str) -> Result<Value, String> {
        byreal_get(
            &self.http,
            &self.url(PATH_PROVIDER_OVERVIEW),
            &[("providerAddress", provider_address.to_string())],
        )
    }
}
