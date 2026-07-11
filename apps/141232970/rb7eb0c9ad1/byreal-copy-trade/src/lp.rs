//! Vendored byreal-lp zap surface — envelope decoder, LpClient, and the
//! open-position route — copied from `apps/byreal-lp/src/{client,tool}.rs`.
//!
//! Platform apps are standalone cdylibs: every app emits the `dyn_aomi_app!`
//! FFI exports, so linking byreal-lp as an rlib duplicates `aomi_create` et
//! al at link time (CI-proven). Vendoring the ~300 shared lines is the
//! platform-correct dependency shape. If byreal's zap contract changes, fix
//! byreal-lp first, then refresh this copy.

#![allow(dead_code)] // vendored subset — unused LpClient methods are kept verbatim

use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;


/// byreal Solana-side API host (same host the byreal app uses).
pub const BYREAL_API_BASE: &str = "https://api2.byreal.io";

const PATH_POOLS_LIST: &str = "/byreal/api/dex/v2/pools/info/list";
const PATH_POOL_DETAILS: &str = "/byreal/api/dex/v2/pools/details";
const PATH_POSITION_LIST: &str = "/byreal/api/dex/v2/position/list";
const PATH_POSITION_UNCLAIMED: &str = "/byreal/api/dex/v2/position/unclaimed-data";

const PATH_ZAP_IN_OPEN_QUOTE: &str =
    "/byreal/api/router/v1/router-service/autoswap/zap-in/open-position/quote";
const PATH_ZAP_IN_OPEN_BUILD: &str =
    "/byreal/api/router/v1/router-service/autoswap/zap-in/open-position/build-tx";
const PATH_ZAP_IN_INC_QUOTE: &str =
    "/byreal/api/router/v1/router-service/autoswap/zap-in/increase-liquidity/quote";
const PATH_ZAP_IN_INC_BUILD: &str =
    "/byreal/api/router/v1/router-service/autoswap/zap-in/increase-liquidity/build-tx";
const PATH_ZAP_OUT_QUOTE: &str = "/byreal/api/router/v1/router-service/autoswap/zap-out/quote";
const PATH_ZAP_OUT_BUILD: &str =
    "/byreal/api/router/v1/router-service/autoswap/zap-out/build-tx";

pub struct LpClient {
    http: reqwest::blocking::Client,
    base_url: String,
}

use std::sync::OnceLock;
static LP_CLIENT: OnceLock<Result<LpClient, String>> = OnceLock::new();

pub fn lp_client() -> Result<&'static LpClient, String> {
    LP_CLIENT
        .get_or_init(LpClient::new)
        .as_ref()
        .map_err(|e| e.clone())
}

impl LpClient {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| format!("[byreal-lp] failed to build HTTP client: {e}"))?,
            base_url: std::env::var("BYREAL_API_URL")
                .unwrap_or_else(|_| BYREAL_API_BASE.to_string()),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    // ---------------------------------------------------------------
    // Reads
    // ---------------------------------------------------------------

    /// `/dex/v2/pools/info/list` — paginated CLMM pool catalog with
    /// TVL/APR/volume, sorted for LP selection.
    pub fn list_pools(
        &self,
        page: u32,
        page_size: u32,
        sort_field: &str,
        sort_type: &str,
        pool_address: Option<&str>,
    ) -> Result<Value, String> {
        let mut q = vec![
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
            ("sortField", sort_field.to_string()),
            ("sortType", sort_type.to_string()),
        ];
        if let Some(a) = pool_address {
            q.push(("poolAddress", a.to_string()));
        }
        byreal_get(&self.http, &self.url(PATH_POOLS_LIST), &q)
    }

    /// `/dex/v2/pools/details` — single pool deep dive (carries the tick
    /// spacing + current price the open-position flow needs).
    pub fn get_pool(&self, pool_address: &str) -> Result<Value, String> {
        byreal_get(
            &self.http,
            &self.url(PATH_POOL_DETAILS),
            &[("poolAddress", pool_address.to_string())],
        )
    }

    /// `/dex/v2/position/list` — CLMM positions held by a wallet.
    pub fn list_positions(&self, wallet: &str) -> Result<Value, String> {
        byreal_get(
            &self.http,
            &self.url(PATH_POSITION_LIST),
            &[("walletAddress", wallet.to_string())],
        )
    }

    /// `/dex/v2/position/unclaimed-data` — unclaimed fees/rewards per position.
    pub fn unclaimed(&self, wallet: &str) -> Result<Value, String> {
        byreal_get(
            &self.http,
            &self.url(PATH_POSITION_UNCLAIMED),
            &[("walletAddress", wallet.to_string())],
        )
    }

    // ---------------------------------------------------------------
    // Zap quotes (router-service). All POST JSON; `slippageBps` is a NUMBER
    // (u16) here, unlike the swap quote where it's a string — verified live.
    // Quotes carry `quoteId` + `quoteContext` + `quoteExpireAtMs`; the id is
    // HMAC-bound with a ~30s TTL, so build must follow quote immediately.
    // ---------------------------------------------------------------

    /// Quote: single input token → new position in `pool` over
    /// `[tick_lower, tick_upper]`. byreal routes the swap-half through its
    /// router (Jupiter et al) and returns the estimated token0/token1 split.
    pub fn zap_in_open_quote(
        &self,
        pool: &str,
        input_mint: &str,
        amount: &str,
        user: &str,
        tick_lower: i32,
        tick_upper: i32,
        slippage_bps: u16,
    ) -> Result<Value, String> {
        byreal_post(
            &self.http,
            &self.url(PATH_ZAP_IN_OPEN_QUOTE),
            &json!({
                "poolAddress": pool,
                "inputMint": input_mint,
                "amount": amount,
                "userPublicKey": user,
                "tickLowerIndex": tick_lower,
                "tickUpperIndex": tick_upper,
                "slippageBps": slippage_bps,
            }),
        )
    }

    /// Quote: single input token → more liquidity in an existing position
    /// (`personal_position` = the position account from `list_positions`).
    pub fn zap_in_increase_quote(
        &self,
        pool: &str,
        input_mint: &str,
        amount: &str,
        user: &str,
        personal_position: &str,
        slippage_bps: u16,
    ) -> Result<Value, String> {
        byreal_post(
            &self.http,
            &self.url(PATH_ZAP_IN_INC_QUOTE),
            &json!({
                "poolAddress": pool,
                "inputMint": input_mint,
                "amount": amount,
                "userPublicKey": user,
                "personalPosition": personal_position,
                "slippageBps": slippage_bps,
            }),
        )
    }

    /// Quote: close/withdraw a position entirely into one output token.
    pub fn zap_out_quote(
        &self,
        pool: &str,
        user: &str,
        personal_position: &str,
        output_mint: &str,
        slippage_bps: u16,
    ) -> Result<Value, String> {
        byreal_post(
            &self.http,
            &self.url(PATH_ZAP_OUT_QUOTE),
            &json!({
                "poolAddress": pool,
                "userPublicKey": user,
                "personalPosition": personal_position,
                "outputMint": output_mint,
                "slippageBps": slippage_bps,
            }),
        )
    }

    // ---------------------------------------------------------------
    // Zap builds — exchange a fresh quote for the unsigned tx blob.
    // ---------------------------------------------------------------

    /// Build the open-position tx. `position_nft_mint` is the pubkey of the
    /// freshly generated ephemeral keypair that will co-sign the tx (the
    /// position NFT mint).
    pub fn zap_in_open_build(
        &self,
        position_nft_mint: &str,
        quote_id: &str,
        quote_context: &Value,
    ) -> Result<Value, String> {
        byreal_post(
            &self.http,
            &self.url(PATH_ZAP_IN_OPEN_BUILD),
            &json!({
                "positionNftMint": position_nft_mint,
                "quoteId": quote_id,
                "quoteContext": quote_context,
            }),
        )
    }

    /// Build the increase-liquidity tx.
    pub fn zap_in_increase_build(
        &self,
        quote_id: &str,
        quote_context: &Value,
    ) -> Result<Value, String> {
        byreal_post(
            &self.http,
            &self.url(PATH_ZAP_IN_INC_BUILD),
            &json!({ "quoteId": quote_id, "quoteContext": quote_context }),
        )
    }

    /// Build the zap-out tx.
    pub fn zap_out_build(
        &self,
        quote_id: &str,
        quote_context: &Value,
    ) -> Result<Value, String> {
        byreal_post(
            &self.http,
            &self.url(PATH_ZAP_OUT_BUILD),
            &json!({ "quoteId": quote_id, "quoteContext": quote_context }),
        )
    }
}

// -------------------------------------------------------------------
// Envelope
// -------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ByrealEnvelope {
    #[serde(rename = "retCode")]
    ret_code: i64,
    #[serde(rename = "retMsg", default)]
    ret_msg: String,
    #[serde(default)]
    result: Option<ByrealEnvelopeResult>,
}

#[derive(serde::Deserialize)]
struct ByrealEnvelopeResult {
    #[serde(default)]
    data: Option<Value>,
    /// Variant 1 (dex/v2): inner success flag; `Some(false)` = failure even
    /// when sibling data is populated.
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    ret_code: Option<i64>,
    #[serde(default)]
    ret_msg: Option<String>,
    /// Variant 2 (router-service): nested `{retCode, retMsg}` status object.
    #[serde(default)]
    result: Option<InnerStatus>,
    // Everything else (quote fields, flattened read payloads).
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

#[derive(serde::Deserialize)]
struct InnerStatus {
    #[serde(rename = "retCode", default)]
    ret_code: i64,
    #[serde(rename = "retMsg", default)]
    ret_msg: String,
}

pub fn byreal_get(
    http: &reqwest::blocking::Client,
    url: &str,
    query: &[(&str, String)],
) -> Result<Value, String> {
    let resp = http
        .get(url)
        .query(query)
        .send()
        .map_err(|e| format!("[byreal-lp] GET {url} failed: {e}"))?;
    decode_envelope_response(resp, url)
}

pub fn byreal_post<B: Serialize>(
    http: &reqwest::blocking::Client,
    url: &str,
    body: &B,
) -> Result<Value, String> {
    let resp = http
        .post(url)
        .json(body)
        .send()
        .map_err(|e| format!("[byreal-lp] POST {url} failed: {e}"))?;
    decode_envelope_response(resp, url)
}

fn decode_envelope_response(
    resp: reqwest::blocking::Response,
    url: &str,
) -> Result<Value, String> {
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("[byreal-lp] {url} returned HTTP {status}: {text}"));
    }
    decode_envelope(&text).map_err(|e| format!("[byreal-lp] {url}: {e}"))
}

/// Decode byreal's envelope, enforcing all three inner-status variants (see
/// module docs). Split from the HTTP layer so envelope semantics are unit-
/// testable without a live response.
fn decode_envelope(text: &str) -> Result<Value, String> {
    let env: ByrealEnvelope = serde_json::from_str(text)
        .map_err(|e| format!("envelope decode failed: {e}; body: {text}"))?;
    if env.ret_code != 0 {
        return Err(format!(
            "returned retCode={} retMsg={}",
            env.ret_code, env.ret_msg
        ));
    }
    let Some(r) = env.result else {
        return Ok(Value::Null);
    };
    // Variant 1: dex/v2 inner success flag.
    if matches!(r.success, Some(false)) {
        let code = r
            .ret_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".into());
        let msg = r.ret_msg.as_deref().unwrap_or("operation failed");
        return Err(format!("inner status failed: ret_code={code} ret_msg={msg}"));
    }
    // Variant 2: router-service nested status object.
    if let Some(inner) = &r.result
        && inner.ret_code != 0
    {
        return Err(format!(
            "inner status failed: ret_code={} ret_msg={}",
            inner.ret_code, inner.ret_msg
        ));
    }
    Ok(r.data.unwrap_or(Value::Object(r.rest)))
}


// ── vendored from byreal-lp tool.rs ──────────────────────────────

use base64::{Engine, engine::general_purpose::STANDARD as B64};

use solana_sdk::{
    pubkey::Pubkey, signature::Signature, signer::Signer, signer::keypair::Keypair,
    transaction::VersionedTransaction,
};
use std::str::FromStr;

use aomi_sdk::*;

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

