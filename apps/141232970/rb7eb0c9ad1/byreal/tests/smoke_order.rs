//! Live smoke test against `https://api.hyperliquid.xyz/exchange`.
//!
//! Requires `BANANA_PRIVATE_KEY` to be set in the environment. The test:
//!   1. Fetches the live ETH mid-price.
//!   2. Places a tiny (~$11 notional) far-OTM (50% below mid) post-only (Alo)
//!      ETH buy with that key.
//!   3. Cancels it immediately.
//!
//! The order CANNOT fill — Alo means post-only, and 50% below mid is well
//! outside the book — so the only thing at risk is exchange fees on a
//! cancellation, which Hyperliquid does not charge for unfilled Alo cancels.
//!
//! Run with:
//!   BANANA_PRIVATE_KEY=0x... cargo test --manifest-path apps/byreal/Cargo.toml \
//!       --test smoke_order -- --ignored --nocapture

use byreal::testing::place_live_smoke_order;
use serde_json::Value;
use std::env;

fn env_string(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[test]
#[ignore = "places a real Hyperliquid order; requires BANANA_PRIVATE_KEY and explicit --ignored"]
fn smoke_place_and_cancel_eth_alo_buy() {
    let Some(_pk) = env_string("BANANA_PRIVATE_KEY") else {
        eprintln!("Skipping live smoke test: BANANA_PRIVATE_KEY is not set.");
        return;
    };

    let result = place_live_smoke_order(None).expect("smoke order helper failed");
    println!(
        "{}",
        serde_json::to_string_pretty(&result).expect("result should serialize")
    );

    // The order MUST have reached /exchange. Whether it rested or was rejected
    // (e.g. for insufficient funds) is informative but not a hard failure.
    let place = result
        .get("place_response")
        .expect("missing place_response in result");
    let status = place
        .get("status")
        .and_then(Value::as_str)
        .expect("place_response missing status");
    assert_eq!(status, "ok", "exchange returned non-ok status: {place:#}");

    // If the order rested, the cleanup cancel must also have come back ok.
    if result.get("cleaned_up").and_then(Value::as_bool) == Some(true) {
        let cancel = result
            .get("cancel_response")
            .expect("cleaned_up=true but no cancel_response");
        let cancel_status = cancel
            .get("status")
            .and_then(Value::as_str)
            .expect("cancel_response missing status");
        assert_eq!(
            cancel_status, "ok",
            "cancel returned non-ok status: {cancel:#}"
        );
    } else {
        eprintln!(
            "WARN: order did not rest on the book (no oid extracted). Inspect place_response for the reason."
        );
    }
}
