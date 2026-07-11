//! Client layer for the byreal-lp app — byreal's CLMM position + AutoSwap
//! (zap) surface on `api2.byreal.io`.
//!
//! One product line, one file:
//! - [`ByrealLpApp`] — the marker struct every `DynAomiTool::App` points to.
//! - Plain HTTP wrappers (`byreal_get` / `byreal_post`) that decode byreal's
//!   envelope and surface non-2xx / inner failures as `Err(String)`.
//! - Endpoint fns for the position reads and the zap quote/build POSTs.
//!
//! ## Envelope — THREE inner-status variants (all observed live)
//!
//! byreal's outer envelope is `{retCode, retMsg, result}`, but the *actual*
//! operation status hides in one of three places depending on the service:
//!
//! 1. `result.success: false` (+ `result.ret_code`/`ret_msg`) — dex/v2
//!    endpoints. Observed returning phantom payloads alongside
//!    `success:false`; must be an Err (same bug class the byreal app guards).
//! 2. `result.result.retCode != 0` (+ `retMsg`) — router-service (zap)
//!    endpoints, e.g. `{"result":{"result":{"retCode":40303,"retMsg":"System
//!    busy…"},"quoteId":null}}`.
//! 3. Neither present — plain read success; payload is `result.data` when
//!    nested, else the flattened `result` object itself.
//!
//! The decoder checks 1 and 2 before extracting, so a failed zap quote can
//! never leak `quoteId: null` fields to the LLM as if they were a quote.

use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct ByrealLpApp;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dex_v2_nested_data_extracts() {
        let body = r#"{"retCode":0,"result":{"success":true,"data":{"records":[],"total":0}}}"#;
        assert_eq!(
            decode_envelope(body).unwrap(),
            serde_json::json!({"records":[],"total":0})
        );
    }

    #[test]
    fn dex_v2_inner_success_false_errs_even_with_data() {
        let body = r#"{"retCode":0,"result":{"success":false,"ret_code":500,"ret_msg":"Internal Server Error","data":["phantom"]}}"#;
        let err = decode_envelope(body).unwrap_err();
        assert!(err.contains("inner status failed"), "{err}");
        assert!(err.contains("500"), "{err}");
    }

    #[test]
    fn router_service_nested_status_ok_flattens_quote_fields() {
        // Live shape from zap-in open quote (trimmed).
        let body = r#"{"retCode":0,"retMsg":"","result":{"result":{"retCode":0,"retMsg":"Success"},"provider":"jupiter","quote":{"swapInAmount":"777573"},"preview":{"estimatedToken0Amount":"222427","estimatedToken1Amount":"778005"},"quoteId":"56ee37a2","quoteContext":{"flowType":"zapInOpenPosition"}}}"#;
        let value = decode_envelope(body).unwrap();
        assert_eq!(value["quoteId"], "56ee37a2");
        assert_eq!(value["preview"]["estimatedToken0Amount"], "222427");
        assert_eq!(value["quoteContext"]["flowType"], "zapInOpenPosition");
    }

    #[test]
    fn router_service_nested_status_failure_errs() {
        // Live shape: schema-valid request against a nonexistent position.
        let body = r#"{"retCode":0,"retMsg":"","result":{"result":{"retCode":40303,"retMsg":"System busy, please try again later"},"provider":null,"preview":null,"quoteId":null,"quoteContext":null,"quoteExpireAtMs":null}}"#;
        let err = decode_envelope(body).unwrap_err();
        assert!(err.contains("40303"), "{err}");
        assert!(!err.contains("quoteId"), "must not leak null quote fields");
    }

    #[test]
    fn router_service_serde_error_string_result_is_a_decode_error() {
        // byreal echoes Rust serde errors as a bare string in `result` —
        // e.g. {"result":Json deserialize error: ...} is INVALID JSON, and
        // even the valid-JSON variant {"result":"..."} must not decode as
        // success. Either way the caller sees an Err.
        let body = r#"{"retCode":0,"retMsg":"","result":"Json deserialize error: missing field `poolAddress`"}"#;
        assert!(decode_envelope(body).is_err());
    }

    #[test]
    fn outer_retcode_nonzero_errs() {
        let body = r#"{"retCode":1001,"retMsg":"invalid signature"}"#;
        let err = decode_envelope(body).unwrap_err();
        assert!(err.contains("retCode=1001"));
    }

    /// Live proof against api2.byreal.io that the read + zap-quote contracts
    /// hold: pools list decodes, and a zap-in open quote on the top pool
    /// returns an actionable `quoteId` + `quoteContext` (the write tools'
    /// entire input). Network-gated (`--ignored`). Uses the system program as
    /// `userPublicKey` — quotes don't check balances.
    #[test]
    #[ignore = "network: hits api2.byreal.io"]
    fn live_pools_and_zap_quote_contract() {
        let client = LpClient::new().expect("client builds");

        let pools = client
            .list_pools(1, 1, "tvl", "desc", None)
            .expect("pools list decodes");
        let pool = pools["records"][0]["poolAddress"]
            .as_str()
            .expect("top pool has an address")
            .to_string();
        let input_mint = pools["records"][0]["mintA"]["mintInfo"]["address"]
            .as_str()
            .expect("pool carries mintA")
            .to_string();

        let quote = client
            .zap_in_open_quote(
                &pool,
                &input_mint,
                "1000000",
                "11111111111111111111111111111111",
                -10,
                10,
                100,
            )
            .expect("zap-in open quote decodes");
        assert!(
            quote["quoteId"].as_str().is_some_and(|s| !s.is_empty()),
            "quote must carry an actionable quoteId: {quote}"
        );
        assert!(
            quote["quoteContext"].is_object(),
            "quote must carry quoteContext for build-tx: {quote}"
        );
    }
}
