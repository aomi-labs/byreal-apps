//! Tool layer for byreal-lp — CLMM position lifecycle via byreal AutoSwap.
//!
//! Reads return envelope-decoded JSON tagged `"source": "byreal-lp"`. Writes
//! are **quote → build → stage → commit in one tool call**: byreal's zap
//! `quoteId` is HMAC-bound with a ~30s TTL (`quoteExpireAtMs`), so splitting
//! quote and build across LLM turns would hand the model expired quotes. The
//! separate `quote_*` tools exist for previews only; each write tool re-quotes
//! internally, exchanges the fresh quote for the unsigned tx blob, and emits
//! the Lane-2 route plan:
//!
//! ```text
//! byreal_clmm_<write> → svm_stage_tx({tx, …}) → svm_commit_tx({tx_id})
//! ```
//!
//! Who signs is kernel policy on the wallet (`SigningMode`); who broadcasts is
//! the staged broadcaster (manifest default `"wallet"`, `"aomi"` allowed for
//! autonomous runs). Commit IS the broadcast — there is no venue submit step
//! (the swap-half routes through byreal's router into Jupiter et al, but the
//! composed tx lands on Solana directly).

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use serde_json::{Value, json};
use solana_sdk::{
    pubkey::Pubkey, signature::Signature, signer::Signer, signer::keypair::Keypair,
    transaction::VersionedTransaction,
};
use std::str::FromStr;

use aomi_sdk::schemars::JsonSchema;
use aomi_sdk::*;

use crate::client::{ByrealLpApp, lp_client};

const DEFAULT_SLIPPAGE_BPS: u16 = 100;
const DEFAULT_PAGE_SIZE: u32 = 10;

/// Tag responses so the LLM can attribute provider when tools are mixed.
fn ok<T: serde::Serialize>(value: T) -> Result<Value, String> {
    let v =
        serde_json::to_value(value).map_err(|e| format!("[byreal-lp] response serialize: {e}"))?;
    Ok(match v {
        Value::Object(mut map) => {
            map.insert("source".to_string(), Value::String("byreal-lp".to_string()));
            Value::Object(map)
        }
        other => json!({ "source": "byreal-lp", "data": other }),
    })
}

/// Resolve the operating SVM wallet: explicit arg wins, else the host context.
fn resolve_wallet(arg: Option<String>, ctx: &DynToolCallCtx) -> Result<String, String> {
    arg.or_else(|| ctx.attribute_string(&["domain", "svm", "address"]))
        .ok_or_else(|| {
            "[byreal-lp] no SVM wallet — pass `wallet` explicitly or connect a Solana wallet"
                .to_string()
        })
}

/// Pull the unsigned base64 tx blob out of a zap build-tx response. byreal
/// names it `transaction` (same as the swap quote); accept close variants and
/// fail with the actual keys so a silent contract change surfaces loudly.
fn extract_tx_blob(build: &Value) -> Result<String, String> {
    for key in ["transaction", "tx", "txBase64"] {
        if let Some(blob) = build.get(key).and_then(Value::as_str)
            && !blob.is_empty()
        {
            return Ok(blob.to_string());
        }
    }
    let keys: Vec<&str> = build
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    Err(format!(
        "[byreal-lp] build-tx response carried no transaction blob (keys: {keys:?}) — \
         byreal may have changed the build contract"
    ))
}

/// Emit the Lane-2 route plan: stage the (possibly partially-signed) blob,
/// then commit — the host resolves signer from kernel policy and submitter
/// from the staged broadcaster. `preserve_blockhash` stays true (default):
/// the blob is venue-built and, for open-position, already co-signed by the
/// ephemeral NFT keypair — replacing the blockhash would invalidate that sig.
fn stage_commit_route(
    preview: Value,
    blob_b64: String,
    description: String,
) -> Result<ToolReturn, String> {
    ToolReturn::route(preview)
        .next(|next| {
            next.add::<host::SvmStageTx>(json!({
                "tx": blob_b64,
                "description": description,
                "kind": "byreal-lp.zap",
            }))
            .bind_as("tx_id")
            .note("Stage the byreal-built zap transaction blob.");
        })
        .after::<host::SvmCommitTx>(json!({}))
        .awaits("tx_id")
        .note(
            "MANDATORY: call svm_commit_tx RIGHT NOW with the staged tx_id. \
             Do NOT generate a text response first. Commit signs under kernel \
             policy and broadcasts — there is no separate submit step.",
        )
        .try_build()
        .map_err(|e| format!("[byreal-lp] route build failed: {e}"))
}

/// Generate the ephemeral position-NFT mint keypair and partially sign the
/// venue-built open-position blob with it. The keypair exists only for this
/// call: it mints the position NFT and has no residual authority afterwards.
/// The wallet's payer signature is added at commit time; byte-stability of
/// the staged blob (preserve_blockhash) keeps this signature valid.
fn sign_with_position_nft(blob_b64: &str, nft: &Keypair) -> Result<String, String> {
    let bytes = B64
        .decode(blob_b64)
        .map_err(|e| format!("[byreal-lp] build-tx blob is not base64: {e}"))?;
    let mut vtx: VersionedTransaction = bincode::deserialize(&bytes)
        .map_err(|e| format!("[byreal-lp] build-tx blob is not a VersionedTransaction: {e}"))?;

    let signer_keys = vtx.message.static_account_keys();
    let num_signers = vtx.message.header().num_required_signatures as usize;
    let nft_pubkey = nft.pubkey();
    let position = signer_keys
        .iter()
        .take(num_signers)
        .position(|k| *k == nft_pubkey)
        .ok_or_else(|| {
            format!(
                "[byreal-lp] position NFT mint {nft_pubkey} is not among the tx's required \
                 signers — byreal's build contract may have changed"
            )
        })?;

    if vtx.signatures.len() < num_signers {
        vtx.signatures.resize(num_signers, Signature::default());
    }
    let message_bytes = vtx.message.serialize();
    vtx.signatures[position] = nft.sign_message(&message_bytes);

    let signed = bincode::serialize(&vtx)
        .map_err(|e| format!("[byreal-lp] re-serialize partially-signed tx failed: {e}"))?;
    Ok(B64.encode(&signed))
}

// ===================================================================
// Reads
// ===================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetPoolsArgs {
    /// Page number, 1-based. Default 1.
    pub page: Option<u32>,
    /// Page size. Default 10.
    pub page_size: Option<u32>,
    /// Sort field: `tvl`, `volumeUsd24h`, `apr24h` (default), `feeUsd24h`.
    pub sort_field: Option<String>,
    /// `desc` (default) or `asc`.
    pub sort: Option<String>,
    /// Filter to one pool address.
    pub pool_address: Option<String>,
}

pub(crate) struct GetPools;

impl DynAomiTool for GetPools {
    type App = ByrealLpApp;
    type Args = GetPoolsArgs;
    const NAME: &'static str = "byreal_clmm_get_pools";
    const DESCRIPTION: &'static str = "List byreal CLMM pools with TVL, APR, volume, and fee metrics — the starting point for choosing where to provide liquidity. Sort by `apr24h` for yield, `tvl` for depth.";

    fn run(_app: &Self::App, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        ok(lp_client()?.list_pools(
            args.page.unwrap_or(1),
            args.page_size.unwrap_or(DEFAULT_PAGE_SIZE),
            args.sort_field.as_deref().unwrap_or("apr24h"),
            args.sort.as_deref().unwrap_or("desc"),
            args.pool_address.as_deref(),
        )?)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetPoolArgs {
    /// The CLMM pool address.
    pub pool_address: String,
}

pub(crate) struct GetPool;

impl DynAomiTool for GetPool {
    type App = ByrealLpApp;
    type Args = GetPoolArgs;
    const NAME: &'static str = "byreal_clmm_get_pool";
    const DESCRIPTION: &'static str = "Deep dive on one byreal CLMM pool — current price/tick, tick spacing, fee rate, TVL, APR breakdown. Read this before opening a position: tick bounds must be multiples of the pool's tick spacing.";

    fn run(_app: &Self::App, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        ok(lp_client()?.get_pool(&args.pool_address)?)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetPositionsArgs {
    /// Wallet to inspect. Defaults to the connected SVM wallet. Pass a top
    /// LP's wallet (from byreal Copy Farming leaderboards) to inspect the
    /// positions you want to mirror.
    pub wallet: Option<String>,
}

pub(crate) struct GetPositions;

impl DynAomiTool for GetPositions {
    type App = ByrealLpApp;
    type Args = GetPositionsArgs;
    const NAME: &'static str = "byreal_clmm_get_positions";
    const DESCRIPTION: &'static str = "List CLMM positions held by a wallet (pool, tick range, liquidity, value). Defaults to the connected wallet; pass another wallet to inspect positions you want to copy. Each record's position account feeds `personal_position` args on increase/zap-out.";

    fn run(_app: &Self::App, args: Self::Args, ctx: DynToolCallCtx) -> Result<Value, String> {
        let wallet = resolve_wallet(args.wallet, &ctx)?;
        ok(lp_client()?.list_positions(&wallet)?)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetUnclaimedArgs {
    /// Wallet to inspect. Defaults to the connected SVM wallet.
    pub wallet: Option<String>,
}

pub(crate) struct GetUnclaimed;

impl DynAomiTool for GetUnclaimed {
    type App = ByrealLpApp;
    type Args = GetUnclaimedArgs;
    const NAME: &'static str = "byreal_clmm_get_unclaimed";
    const DESCRIPTION: &'static str = "Unclaimed fees and incentive rewards across a wallet's byreal CLMM positions. Read-only; claiming itself is the byreal app's `byreal_lp_build_claim_rewards` flow.";

    fn run(_app: &Self::App, args: Self::Args, ctx: DynToolCallCtx) -> Result<Value, String> {
        let wallet = resolve_wallet(args.wallet, &ctx)?;
        ok(lp_client()?.unclaimed(&wallet)?)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct QuoteZapInArgs {
    /// Target CLMM pool address.
    pub pool_address: String,
    /// Mint of the single input token you're zapping in with.
    pub input_mint: String,
    /// Input amount in the token's atomic units (e.g. "50000000" = 50 USDC).
    pub amount: String,
    /// Lower tick index for a NEW position (must be a multiple of the pool's
    /// tick spacing). Omit both ticks when quoting an increase.
    pub tick_lower_index: Option<i32>,
    /// Upper tick index for a NEW position.
    pub tick_upper_index: Option<i32>,
    /// Existing position account to increase (from `byreal_clmm_get_positions`).
    /// Mutually exclusive with the tick bounds.
    pub personal_position: Option<String>,
    /// Slippage in bps. Default 100 (1%).
    pub slippage_bps: Option<u16>,
    /// Wallet paying. Defaults to the connected SVM wallet.
    pub wallet: Option<String>,
}

pub(crate) struct QuoteZapIn;

impl DynAomiTool for QuoteZapIn {
    type App = ByrealLpApp;
    type Args = QuoteZapInArgs;
    const NAME: &'static str = "byreal_clmm_quote_zap_in";
    const DESCRIPTION: &'static str = "PREVIEW a zap-in: how a single input token would split into a CLMM position (swap route, price impact, estimated token0/token1). For a new position pass `tick_lower_index`+`tick_upper_index`; to top up an existing one pass `personal_position`. Quotes expire in ~30s — the write tools re-quote internally, so use this for showing the user numbers, not for caching a quoteId.";

    fn run(_app: &Self::App, args: Self::Args, ctx: DynToolCallCtx) -> Result<Value, String> {
        let wallet = resolve_wallet(args.wallet, &ctx)?;
        let slippage = args.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
        let client = lp_client()?;
        let quote = match (
            args.personal_position.as_deref(),
            args.tick_lower_index,
            args.tick_upper_index,
        ) {
            (Some(position), None, None) => client.zap_in_increase_quote(
                &args.pool_address,
                &args.input_mint,
                &args.amount,
                &wallet,
                position,
                slippage,
            )?,
            (None, Some(lower), Some(upper)) => client.zap_in_open_quote(
                &args.pool_address,
                &args.input_mint,
                &args.amount,
                &wallet,
                lower,
                upper,
                slippage,
            )?,
            _ => {
                return Err(
                    "[byreal-lp] pass EITHER tick_lower_index + tick_upper_index (new position) \
                     OR personal_position (increase) — not both, not neither"
                        .to_string(),
                );
            }
        };
        ok(quote)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct QuoteZapOutArgs {
    /// The position's pool address.
    pub pool_address: String,
    /// Position account to exit (from `byreal_clmm_get_positions`).
    pub personal_position: String,
    /// Mint of the single token you want back out.
    pub output_mint: String,
    /// Slippage in bps. Default 100 (1%).
    pub slippage_bps: Option<u16>,
    /// Wallet owning the position. Defaults to the connected SVM wallet.
    pub wallet: Option<String>,
}

pub(crate) struct QuoteZapOut;

impl DynAomiTool for QuoteZapOut {
    type App = ByrealLpApp;
    type Args = QuoteZapOutArgs;
    const NAME: &'static str = "byreal_clmm_quote_zap_out";
    const DESCRIPTION: &'static str = "PREVIEW a zap-out: what closing a CLMM position entirely into one output token would return (swap route, price impact, estimated amount). Quotes expire in ~30s — `byreal_clmm_zap_out_position` re-quotes internally.";

    fn run(_app: &Self::App, args: Self::Args, ctx: DynToolCallCtx) -> Result<Value, String> {
        let wallet = resolve_wallet(args.wallet, &ctx)?;
        ok(lp_client()?.zap_out_quote(
            &args.pool_address,
            &wallet,
            &args.personal_position,
            &args.output_mint,
            args.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
        )?)
    }
}

// ===================================================================
// Writes — quote + build + stage/commit in ONE call (quote TTL ~30s)
// ===================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct OpenPositionArgs {
    /// Target CLMM pool address.
    pub pool_address: String,
    /// Mint of the single input token (the zap swaps + splits it).
    pub input_mint: String,
    /// Input amount in atomic units.
    pub amount: String,
    /// Lower tick index — a multiple of the pool's tick spacing (read
    /// `byreal_clmm_get_pool`). To mirror another LP, reuse their position's
    /// tick bounds from `byreal_clmm_get_positions`.
    pub tick_lower_index: i32,
    /// Upper tick index.
    pub tick_upper_index: i32,
    /// Slippage in bps. Default 100 (1%).
    pub slippage_bps: Option<u16>,
}

pub(crate) struct OpenPosition;

impl DynAomiTool for OpenPosition {
    type App = ByrealLpApp;
    type Args = OpenPositionArgs;
    const NAME: &'static str = "byreal_clmm_open_position";
    const DESCRIPTION: &'static str = "OPEN a new byreal CLMM position from a single input token (zap-in). Re-quotes internally (quote TTL ~30s), generates the position-NFT mint keypair, pre-signs the venue-built tx with it, and stages it for the wallet's payer signature via svm_stage_tx → svm_commit_tx. Surface a confirmation summary (pool, ticks, amount, estimated split, price impact) and get explicit go-ahead BEFORE calling — unless the conversation is pre-authorized.";

    fn run_with_routes(
        _app: &Self::App,
        args: Self::Args,
        ctx: DynToolCallCtx,
    ) -> Result<ToolReturn, String> {
        let wallet = resolve_wallet(None, &ctx)?;
        open_position_route(
            &wallet,
            &args.pool_address,
            &args.input_mint,
            &args.amount,
            args.tick_lower_index,
            args.tick_upper_index,
            args.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
        )
    }
}

/// Open a new byreal CLMM position from a single input token — the full
/// quote → build → NFT-sign → stage/commit route, as one reusable step.
///
/// Public so a composing app (`byreal-copy-trade`'s mirror tool) can open a
/// position at another LP's tick bounds without re-deriving byreal's zap
/// contract. Re-quotes internally, so the ~30s quote TTL is never the
/// caller's problem.
pub fn open_position_route(
    wallet: &str,
    pool: &str,
    input_mint: &str,
    amount: &str,
    tick_lower: i32,
    tick_upper: i32,
    slippage_bps: u16,
) -> Result<ToolReturn, String> {
    Pubkey::from_str(wallet)
        .map_err(|e| format!("[byreal-lp] operating wallet is not base58: {e}"))?;
    let client = lp_client()?;

    // Fresh quote (the TTL makes any earlier preview quote unusable).
    let quote =
        client.zap_in_open_quote(pool, input_mint, amount, wallet, tick_lower, tick_upper, slippage_bps)?;
    let (quote_id, quote_context) = quote_parts(&quote)?;

    // Ephemeral position-NFT mint: co-signer of the open tx, no residual
    // authority. Generated here, signs below, dropped at return.
    let nft = Keypair::new();
    let build = client.zap_in_open_build(&nft.pubkey().to_string(), quote_id, quote_context)?;
    let blob = extract_tx_blob(&build)?;
    let signed_blob = sign_with_position_nft(&blob, &nft)?;

    let preview = json!({
        "action_kind": "byreal_clmm_open_position",
        "pool": pool,
        "wallet": wallet,
        "input": { "mint": input_mint, "amount": amount },
        "ticks": { "lower": tick_lower, "upper": tick_upper },
        "slippage_bps": slippage_bps,
        "position_nft_mint": nft.pubkey().to_string(),
        "quote": quote.get("quote").cloned().unwrap_or(Value::Null),
        "estimated_split": quote.get("preview").cloned().unwrap_or(Value::Null),
        "source": "byreal-lp",
    });
    stage_commit_route(
        preview,
        signed_blob,
        format!("byreal-lp: open CLMM position in {pool} (ticks {tick_lower}..{tick_upper}), zap {amount} of {input_mint}"),
    )
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct IncreasePositionArgs {
    /// The position's pool address.
    pub pool_address: String,
    /// Existing position account (from `byreal_clmm_get_positions`).
    pub personal_position: String,
    /// Mint of the single input token.
    pub input_mint: String,
    /// Input amount in atomic units.
    pub amount: String,
    /// Slippage in bps. Default 100 (1%).
    pub slippage_bps: Option<u16>,
}

pub(crate) struct IncreasePosition;

impl DynAomiTool for IncreasePosition {
    type App = ByrealLpApp;
    type Args = IncreasePositionArgs;
    const NAME: &'static str = "byreal_clmm_increase_position";
    const DESCRIPTION: &'static str = "ADD liquidity to an existing byreal CLMM position from a single input token (zap-in). Re-quotes internally and stages the venue-built tx via svm_stage_tx → svm_commit_tx. Surface a confirmation summary and get explicit go-ahead BEFORE calling — unless the conversation is pre-authorized.";

    fn run_with_routes(
        _app: &Self::App,
        args: Self::Args,
        ctx: DynToolCallCtx,
    ) -> Result<ToolReturn, String> {
        let wallet = resolve_wallet(None, &ctx)?;
        let slippage = args.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
        let client = lp_client()?;

        let quote = client.zap_in_increase_quote(
            &args.pool_address,
            &args.input_mint,
            &args.amount,
            &wallet,
            &args.personal_position,
            slippage,
        )?;
        let (quote_id, quote_context) = quote_parts(&quote)?;
        let build = client.zap_in_increase_build(quote_id, quote_context)?;
        let blob = extract_tx_blob(&build)?;

        let preview = json!({
            "action_kind": "byreal_clmm_increase_position",
            "pool": args.pool_address,
            "position": args.personal_position,
            "wallet": wallet,
            "input": { "mint": args.input_mint, "amount": args.amount },
            "slippage_bps": slippage,
            "quote": quote.get("quote").cloned().unwrap_or(Value::Null),
            "estimated_split": quote.get("preview").cloned().unwrap_or(Value::Null),
            "source": "byreal-lp",
        });
        stage_commit_route(
            preview,
            blob,
            format!(
                "byreal-lp: increase position {} with {} of {}",
                args.personal_position, args.amount, args.input_mint
            ),
        )
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ZapOutPositionArgs {
    /// The position's pool address.
    pub pool_address: String,
    /// Position account to exit (from `byreal_clmm_get_positions`).
    pub personal_position: String,
    /// Mint of the single token to receive.
    pub output_mint: String,
    /// Slippage in bps. Default 100 (1%).
    pub slippage_bps: Option<u16>,
}

pub(crate) struct ZapOutPosition;

impl DynAomiTool for ZapOutPosition {
    type App = ByrealLpApp;
    type Args = ZapOutPositionArgs;
    const NAME: &'static str = "byreal_clmm_zap_out_position";
    const DESCRIPTION: &'static str = "CLOSE a byreal CLMM position entirely into one output token (zap-out): withdraws both sides, swaps to `output_mint`, closes the position. Re-quotes internally and stages the venue-built tx via svm_stage_tx → svm_commit_tx. Surface a confirmation summary and get explicit go-ahead BEFORE calling — unless the conversation is pre-authorized.";

    fn run_with_routes(
        _app: &Self::App,
        args: Self::Args,
        ctx: DynToolCallCtx,
    ) -> Result<ToolReturn, String> {
        let wallet = resolve_wallet(None, &ctx)?;
        let slippage = args.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
        let client = lp_client()?;

        let quote = client.zap_out_quote(
            &args.pool_address,
            &wallet,
            &args.personal_position,
            &args.output_mint,
            slippage,
        )?;
        let (quote_id, quote_context) = quote_parts(&quote)?;
        let build = client.zap_out_build(quote_id, quote_context)?;
        let blob = extract_tx_blob(&build)?;

        let preview = json!({
            "action_kind": "byreal_clmm_zap_out_position",
            "pool": args.pool_address,
            "position": args.personal_position,
            "wallet": wallet,
            "output_mint": args.output_mint,
            "slippage_bps": slippage,
            "quote": quote.get("quote").cloned().unwrap_or(Value::Null),
            "estimated_out": quote.get("preview").cloned().unwrap_or(Value::Null),
            "source": "byreal-lp",
        });
        stage_commit_route(
            preview,
            blob,
            format!(
                "byreal-lp: zap out position {} to {}",
                args.personal_position, args.output_mint
            ),
        )
    }
}

/// Pull `quoteId` + `quoteContext` from a quote response — both are required
/// by every build endpoint; a quote without them is not actionable.
fn quote_parts(quote: &Value) -> Result<(&str, &Value), String> {
    let quote_id = quote
        .get("quoteId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "[byreal-lp] quote response carried no quoteId".to_string())?;
    let quote_context = quote
        .get("quoteContext")
        .filter(|v| !v.is_null())
        .ok_or_else(|| "[byreal-lp] quote response carried no quoteContext".to_string())?;
    Ok((quote_id, quote_context))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aomi_sdk::testing::TestCtxBuilder;
    use solana_sdk::{
        hash::Hash,
        instruction::{AccountMeta, Instruction},
        message::{Message, VersionedMessage},
    };

    fn ctx_with_wallet(addr: &str) -> DynToolCallCtx {
        TestCtxBuilder::new("byreal_clmm_get_positions")
            .attribute(
                "domain",
                serde_json::json!({ "svm": { "address": addr, "cluster": "solana:mainnet" } }),
            )
            .build()
    }

    #[test]
    fn resolve_wallet_prefers_arg_then_ctx() {
        let ctx = ctx_with_wallet("CtxWallet1111111111111111111111");
        assert_eq!(
            resolve_wallet(Some("ArgWallet".into()), &ctx).unwrap(),
            "ArgWallet"
        );
        assert_eq!(
            resolve_wallet(None, &ctx).unwrap(),
            "CtxWallet1111111111111111111111"
        );
        let empty = TestCtxBuilder::new("t")
            .attribute("domain", serde_json::json!({}))
            .build();
        assert!(resolve_wallet(None, &empty).is_err());
    }

    #[test]
    fn quote_parts_requires_id_and_context() {
        let good = json!({ "quoteId": "abc", "quoteContext": { "flowType": "zapInOpenPosition" } });
        let (id, ctx) = quote_parts(&good).unwrap();
        assert_eq!(id, "abc");
        assert_eq!(ctx["flowType"], "zapInOpenPosition");
        // Null / absent → actionable errors, not panics.
        assert!(quote_parts(&json!({ "quoteId": null })).is_err());
        assert!(quote_parts(&json!({ "quoteId": "abc", "quoteContext": null })).is_err());
    }

    #[test]
    fn extract_tx_blob_reads_known_keys_and_reports_unknown() {
        assert_eq!(
            extract_tx_blob(&json!({ "transaction": "AQAB" })).unwrap(),
            "AQAB"
        );
        assert_eq!(extract_tx_blob(&json!({ "tx": "AQAB" })).unwrap(), "AQAB");
        let err = extract_tx_blob(&json!({ "somethingElse": 1 })).unwrap_err();
        assert!(err.contains("somethingElse"), "{err}");
    }

    /// Build a 2-signer tx (payer + nft) the way byreal's build-tx would, and
    /// verify the partial sign lands the NFT signature in the right slot
    /// while leaving the payer slot untouched.
    #[test]
    fn position_nft_partial_sign_fills_only_its_slot() {
        let payer = Pubkey::new_unique();
        let nft = Keypair::new();
        let program = Pubkey::new_unique();

        let ix = Instruction {
            program_id: program,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(nft.pubkey(), true),
            ],
            data: vec![1, 2, 3],
        };
        let mut message = Message::new(&[ix], Some(&payer));
        message.recent_blockhash = Hash::new_unique();
        let vtx = VersionedTransaction {
            signatures: vec![Signature::default(), Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        let blob = B64.encode(bincode::serialize(&vtx).unwrap());

        let signed_blob = sign_with_position_nft(&blob, &nft).unwrap();
        let signed: VersionedTransaction =
            bincode::deserialize(&B64.decode(&signed_blob).unwrap()).unwrap();

        // Payer slot (index 0) untouched; NFT slot (index 1) carries a valid
        // signature over the message bytes.
        assert_eq!(signed.signatures[0], Signature::default());
        let message_bytes = signed.message.serialize();
        assert!(
            signed.signatures[1].verify(nft.pubkey().as_ref(), &message_bytes),
            "NFT signature must verify against the message"
        );
    }

    #[test]
    fn position_nft_sign_fails_loud_when_not_a_signer() {
        let payer = Pubkey::new_unique();
        let stranger = Keypair::new();
        let program = Pubkey::new_unique();
        let ix = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(payer, true)],
            data: vec![],
        };
        let mut message = Message::new(&[ix], Some(&payer));
        message.recent_blockhash = Hash::new_unique();
        let vtx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        let blob = B64.encode(bincode::serialize(&vtx).unwrap());
        let err = sign_with_position_nft(&blob, &stranger).unwrap_err();
        assert!(err.contains("not among the tx's required signers"), "{err}");
    }
}
