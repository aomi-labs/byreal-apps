//! Tool layer for byreal-copy-trade — discover top LPs, then mirror one's
//! position into your own wallet.
//!
//! Reads use this app's `copyfarmer` client plus byreal-lp's position reads.
//! The single write, `byreal_copy_mirror_position`, is pure composition: it
//! forwards to `byreal_lp::tool::open_position_route`, which runs the whole
//! quote → build → NFT-sign → svm_stage_tx → svm_commit_tx flow. Copy-trade
//! adds no new transaction contract — it opens a byreal CLMM position at the
//! *tick range you copied from a farmer*.

use serde::Deserialize;
use serde_json::{Value, json};

use aomi_sdk::schemars::JsonSchema;
use aomi_sdk::*;

use crate::client::{ByrealCopyTradeApp, copy_client};
use byreal_lp::client::lp_client;
use byreal_lp::tool::open_position_route;

const DEFAULT_SLIPPAGE_BPS: u16 = 100;
const DEFAULT_PAGE_SIZE: u32 = 10;

/// Valid `sortField` values byreal's `top-positions` enum accepts.
const SORT_FIELDS: [&str; 9] = [
    "pnl", "earned", "apr", "liquidity", "copies", "bonus", "openTime", "closeTime", "age",
];

fn ok<T: serde::Serialize>(value: T) -> Result<Value, String> {
    let v = serde_json::to_value(value)
        .map_err(|e| format!("[byreal-copy-trade] response serialize: {e}"))?;
    Ok(match v {
        Value::Object(mut map) => {
            map.insert(
                "source".to_string(),
                Value::String("byreal-copy-trade".to_string()),
            );
            Value::Object(map)
        }
        other => json!({ "source": "byreal-copy-trade", "data": other }),
    })
}

fn resolve_wallet(arg: Option<String>, ctx: &DynToolCallCtx) -> Result<String, String> {
    arg.or_else(|| ctx.attribute_string(&["domain", "svm", "address"]))
        .ok_or_else(|| {
            "[byreal-copy-trade] no SVM wallet — pass `wallet` or connect a Solana wallet"
                .to_string()
        })
}

// ===================================================================
// Discovery reads
// ===================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetTopFarmersArgs {
    /// Sort field: `pnl` (default), `earned`, `apr`, `liquidity`, `copies`,
    /// `bonus`, `openTime`, `closeTime`, `age`.
    pub sort_field: Option<String>,
    /// `desc` (default) or `asc`.
    pub sort: Option<String>,
    /// Restrict the leaderboard to one pool.
    pub pool_address: Option<String>,
    /// Page number, 1-based. Default 1.
    pub page: Option<u32>,
    /// Page size. Default 10.
    pub page_size: Option<u32>,
}

pub(crate) struct GetTopFarmers;

impl DynAomiTool for GetTopFarmers {
    type App = ByrealCopyTradeApp;
    type Args = GetTopFarmersArgs;
    const NAME: &'static str = "byreal_copy_get_top_farmers";
    const DESCRIPTION: &'static str = "The byreal Copy-Farming leaderboard: top LP positions ranked by `pnl` (risk-adjusted), `earned` (raw fees), `apr`, or `liquidity` (whales). Each record is a live position carrying its `poolAddress`, `lowerTick`/`upperTick`, and the provider's wallet — feed those straight into `byreal_copy_mirror_position` to copy it. Start here.";

    fn run(_app: &Self::App, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        let sort_field = args.sort_field.as_deref().unwrap_or("pnl");
        if !SORT_FIELDS.contains(&sort_field) {
            return Err(format!(
                "[byreal-copy-trade] sort_field must be one of {SORT_FIELDS:?}, got {sort_field:?}"
            ));
        }
        ok(copy_client()?.list_top_positions(
            args.page.unwrap_or(1),
            args.page_size.unwrap_or(DEFAULT_PAGE_SIZE),
            sort_field,
            args.sort.as_deref().unwrap_or("desc"),
            args.pool_address.as_deref(),
        )?)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetProviderArgs {
    /// The LP's wallet address (the `providerAddress` from a leaderboard row).
    pub provider_address: String,
}

pub(crate) struct GetProvider;

impl DynAomiTool for GetProvider {
    type App = ByrealCopyTradeApp;
    type Args = GetProviderArgs;
    const NAME: &'static str = "byreal_copy_get_provider";
    const DESCRIPTION: &'static str = "Aggregate stats for one LP (follower/copy count, cumulative earned + pnl) — the due-diligence read before mirroring their position. Pair with `byreal_copy_get_provider_positions` to see the exact positions they hold.";

    fn run(_app: &Self::App, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        ok(copy_client()?.provider_overview(&args.provider_address)?)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetProviderPositionsArgs {
    /// The LP's wallet address to inspect.
    pub provider_address: String,
}

pub(crate) struct GetProviderPositions;

impl DynAomiTool for GetProviderPositions {
    type App = ByrealCopyTradeApp;
    type Args = GetProviderPositionsArgs;
    const NAME: &'static str = "byreal_copy_get_provider_positions";
    const DESCRIPTION: &'static str = "The full set of CLMM positions a given LP wallet currently holds (pool, tick range, liquidity, value) — deeper than one leaderboard row. Pick the position whose pool + tick range you want to mirror.";

    fn run(_app: &Self::App, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        // Reuse byreal-lp's position read — same endpoint, same decoder.
        ok(lp_client()?.list_positions(&args.provider_address)?)
    }
}

// ===================================================================
// The marquee write — mirror a farmer's position
// ===================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct MirrorPositionArgs {
    /// Pool of the position you're copying (from a leaderboard row's
    /// `poolAddress`).
    pub pool_address: String,
    /// The copied position's lower tick (`lowerTick`). Mirrored verbatim — it
    /// is already aligned to the pool's tick spacing.
    pub lower_tick: i32,
    /// The copied position's upper tick (`upperTick`).
    pub upper_tick: i32,
    /// Mint of the single token you're funding your mirror with.
    pub input_mint: String,
    /// Amount in the input token's atomic units (e.g. "50000000" = 50 USDC).
    /// Size to YOUR budget, not the farmer's.
    pub amount: String,
    /// Slippage in bps. Default 100 (1%).
    pub slippage_bps: Option<u16>,
    /// Wallet to open the mirror in. Defaults to the connected SVM wallet.
    pub wallet: Option<String>,
}

pub(crate) struct MirrorPosition;

impl DynAomiTool for MirrorPosition {
    type App = ByrealCopyTradeApp;
    type Args = MirrorPositionArgs;
    const NAME: &'static str = "byreal_copy_mirror_position";
    const DESCRIPTION: &'static str = "COPY a farmer's byreal CLMM position: open a new position in the SAME pool over the SAME tick range (`lower_tick`/`upper_tick` from a leaderboard row), funded from a single input token and sized to your budget. Zap-in under the hood (quote → build → svm_stage_tx → svm_commit_tx). Surface a confirmation summary (which farmer/pool, tick range, your amount, estimated split) and get explicit go-ahead BEFORE calling — unless the conversation is pre-authorized.";

    fn run_with_routes(
        _app: &Self::App,
        args: Self::Args,
        ctx: DynToolCallCtx,
    ) -> Result<ToolReturn, String> {
        if args.lower_tick >= args.upper_tick {
            return Err(format!(
                "[byreal-copy-trade] lower_tick ({}) must be below upper_tick ({}) — copy them \
                 verbatim from the leaderboard row",
                args.lower_tick, args.upper_tick
            ));
        }
        let wallet = resolve_wallet(args.wallet, &ctx)?;
        // Pure composition: byreal-lp owns the zap contract + NFT signing.
        open_position_route(
            &wallet,
            &args.pool_address,
            &args.input_mint,
            &args.amount,
            args.lower_tick,
            args.upper_tick,
            args.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aomi_sdk::testing::TestCtxBuilder;

    #[test]
    fn top_farmers_rejects_unknown_sort_field() {
        let ctx = TestCtxBuilder::new("byreal_copy_get_top_farmers").build();
        let args = GetTopFarmersArgs {
            sort_field: Some("earnedUsd".into()), // byreal's enum doesn't accept this
            sort: None,
            pool_address: None,
            page: None,
            page_size: None,
        };
        let err = GetTopFarmers::run(&ByrealCopyTradeApp, args, ctx).unwrap_err();
        assert!(err.contains("sort_field must be one of"), "{err}");
    }

    #[test]
    fn mirror_rejects_inverted_ticks_before_any_network_call() {
        let ctx = TestCtxBuilder::new("byreal_copy_mirror_position")
            .attribute(
                "domain",
                serde_json::json!({ "svm": { "address": "Wallet1111111111111111111111111" } }),
            )
            .build();
        let args = MirrorPositionArgs {
            pool_address: "Pool".into(),
            lower_tick: 100,
            upper_tick: 100, // not strictly below → rejected
            input_mint: "Usdc".into(),
            amount: "1000000".into(),
            slippage_bps: None,
            wallet: None,
        };
        let err = MirrorPosition::run_with_routes(&ByrealCopyTradeApp, args, ctx).unwrap_err();
        assert!(err.contains("must be below upper_tick"), "{err}");
    }

    #[test]
    fn mirror_requires_a_wallet() {
        let ctx = TestCtxBuilder::new("byreal_copy_mirror_position")
            .attribute("domain", serde_json::json!({}))
            .build();
        let args = MirrorPositionArgs {
            pool_address: "Pool".into(),
            lower_tick: -100,
            upper_tick: 100,
            input_mint: "Usdc".into(),
            amount: "1000000".into(),
            slippage_bps: None,
            wallet: None,
        };
        let err = MirrorPosition::run_with_routes(&ByrealCopyTradeApp, args, ctx).unwrap_err();
        assert!(err.contains("no SVM wallet"), "{err}");
    }

    /// Live proof against api2.byreal.io that the leaderboard contract holds
    /// and every row is directly mirrorable: each carries a `poolAddress` and
    /// `lowerTick` < `upperTick` — the exact args `mirror_position` needs.
    /// Network-gated (`--ignored`).
    #[test]
    #[ignore = "network: hits api2.byreal.io"]
    fn live_leaderboard_rows_are_mirrorable() {
        let top = copy_client()
            .unwrap()
            .list_top_positions(1, 3, "pnl", "desc", None)
            .expect("top-positions decodes");
        let records = top["records"].as_array().expect("records array");
        assert!(!records.is_empty(), "leaderboard should have rows");
        for row in records {
            assert!(
                row["poolAddress"].as_str().is_some_and(|s| !s.is_empty()),
                "row missing poolAddress: {row}"
            );
            let lower = row["lowerTick"].as_i64().expect("lowerTick");
            let upper = row["upperTick"].as_i64().expect("upperTick");
            assert!(lower < upper, "row ticks not ordered: {lower}..{upper}");
        }
    }
}
