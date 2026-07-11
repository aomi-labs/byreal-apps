//! Live read-only smoke test against `https://api2.byreal.io`.
//!
//! Verifies our envelope unwrap, URL construction, and response decoding all
//! match what byreal actually returns. NO signing, NO funds, NO env vars
//! required — but `#[ignore]` because it depends on byreal's API being up.
//!
//! Run with:
//!   cargo test --manifest-path apps/byreal/Cargo.toml \
//!       --test byreal_solana_smoke -- --ignored --nocapture

use byreal::testing::{
    smoke_byreal_lp_top_performers, smoke_byreal_spot_reads, smoke_byreal_swap_quote,
};
use serde_json::Value;

/// Probe address used as `userPublicKey` in the swap quote. byreal pre-builds
/// the unsigned tx for this signer, so it must be a syntactically valid
/// Solana base58 pubkey — the System Program's address (all-1s) works fine
/// as a placeholder; we never sign anything here.
const QUOTE_PROBE_WALLET: &str = "11111111111111111111111111111111";

#[test]
#[ignore = "hits live byreal mainnet API; run with --ignored"]
fn spot_reads_decode_cleanly() {
    let v = smoke_byreal_spot_reads().expect("spot reads should succeed");
    println!(
        "{}",
        serde_json::to_string_pretty(&v).expect("result should serialize")
    );
    // Sanity: pools list should carry a non-empty `records` array (even on a
    // quiet day, byreal has tens of pools).
    let pools = v
        .get("pools_top5_by_tvl")
        .expect("missing pools field in result");
    let records = pools
        .get("records")
        .and_then(Value::as_array)
        .expect("pools response missing `records` array");
    assert!(!records.is_empty(), "expected at least one pool record");
}

#[test]
#[ignore = "hits live byreal mainnet API; run with --ignored"]
fn swap_quote_returns_unsigned_tx() {
    let v = smoke_byreal_swap_quote(QUOTE_PROBE_WALLET).expect("swap quote should succeed");
    println!(
        "{}",
        serde_json::to_string_pretty(&v).expect("result should serialize")
    );
    let tx_len = v
        .get("unsigned_tx_len_b64")
        .and_then(Value::as_u64)
        .expect("missing unsigned_tx_len_b64");
    assert!(
        tx_len > 100,
        "unsigned tx looks too small to be a real Solana tx: {tx_len} chars"
    );
    let router = v
        .get("router_type")
        .and_then(Value::as_str)
        .expect("missing router_type");
    assert!(
        router == "AMM" || router == "RFQ",
        "unexpected routerType: {router}"
    );
}

#[test]
#[ignore = "hits live byreal mainnet API; run with --ignored"]
fn copy_farming_top_performers_returns_records() {
    let v =
        smoke_byreal_lp_top_performers().expect("byreal_lp_top_performers smoke should succeed");
    println!(
        "{}",
        serde_json::to_string_pretty(&v).expect("result should serialize")
    );
    let records = v
        .get("records")
        .and_then(Value::as_array)
        .expect("top positions response missing `records`");
    assert!(
        !records.is_empty(),
        "expected the byreal Copy Farmer leaderboard to have at least one entry"
    );
}
