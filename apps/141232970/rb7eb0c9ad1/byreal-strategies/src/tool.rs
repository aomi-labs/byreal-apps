//! Tool layer for byreal-strategies — curated one-tap standing strategies.
//!
//! A recipe here is pure **rendering**: `byreal_strategy_start` turns a named
//! strategy + params into a fully-formed intent + schedule, then emits a route
//! plan whose next step is the HOST scheduler (`schedule_cron` for cadences,
//! `wake_on_condition` for price triggers) with the rendered args — including
//! `application`, so the armed job fires under the venue app that carries the
//! right tools (`jupiter`, `byreal-lp`, `byreal`), not under this app.
//!
//! The app persists nothing and signs nothing: durability, firing, judging,
//! and signing all belong to the host. This app owns only the byreal-flavored
//! strategy content — which is exactly why it lives on the byreal platform
//! and not in the aomi-core namespace.
//!
//! ── CONTENT IS A DRAFT ─────────────────────────────────────────────────────
//! Recipe wording, guardrails, and defaults are product content pending
//! sign-off; they live in data + pure `render_*` functions (unit-tested) so
//! they can be reshaped without touching the routing.

use serde::Deserialize;
use serde_json::{Value, json};

use aomi_sdk::schemars::JsonSchema;
use aomi_sdk::*;

use crate::client::ByrealStrategiesApp;

/// Default guard re-check cadence for condition recipes, seconds.
const DEFAULT_POLL_SECONDS: i64 = 60;
/// Default swap slippage for the DCA recipe, bps.
const DEFAULT_DCA_SLIPPAGE_BPS: i64 = 50;
/// Default idle-yield position width, tick-spacings either side of spot.
const DEFAULT_YIELD_SPACINGS: i64 = 10;

pub(crate) struct RecipeInfo {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// The venue app the armed strategy fires under.
    pub app: &'static str,
}

pub(crate) const RECIPES: &[RecipeInfo] = &[
    RecipeInfo {
        id: "dca",
        title: "Dollar-cost average",
        summary: "Buy a fixed amount of a token on a cadence via best-execution swap.",
        app: "jupiter",
    },
    RecipeInfo {
        id: "idle-yield",
        title: "Idle-yield sweep",
        summary: "Park idle stablecoin into the top byreal CLMM pool on a cadence.",
        app: "byreal-lp",
    },
    RecipeInfo {
        id: "stop-loss",
        title: "Stop-loss",
        summary: "Sell an asset when its price falls to a threshold.",
        app: "byreal",
    },
    RecipeInfo {
        id: "take-profit",
        title: "Take-profit",
        summary: "Sell an asset when its price rises to a target.",
        app: "byreal",
    },
];

fn find_recipe(id: &str) -> Option<&'static RecipeInfo> {
    RECIPES.iter().find(|r| r.id == id)
}

/// What a recipe renders to: the scheduler tool to call and its full args.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rendered {
    /// `schedule_cron` or `wake_on_condition`.
    pub scheduler: &'static str,
    /// Complete args for that host tool (intent, application, timing/condition).
    pub args: Value,
    /// The venue app the job fires under (also inside `args.application`).
    pub app: &'static str,
}

pub(crate) fn render(recipe: &'static RecipeInfo, p: &Value, now: i64) -> Result<Rendered, String> {
    match recipe.id {
        "dca" => render_dca(recipe, p, now),
        "idle-yield" => render_idle_yield(recipe, p, now),
        "stop-loss" => render_threshold(recipe, p, Side::StopLoss),
        "take-profit" => render_threshold(recipe, p, Side::TakeProfit),
        other => Err(format!("no renderer for recipe '{other}'")),
    }
}

fn render_dca(recipe: &'static RecipeInfo, p: &Value, now: i64) -> Result<Rendered, String> {
    let spend = req_str(p, "spend_mint")?;
    let buy = req_str(p, "buy_mint")?;
    let amount = req_str(p, "amount")?; // atomic units
    let cadence = opt_str(p, "cadence").unwrap_or_else(|| "daily".into());
    let recurrence = cadence_seconds(&cadence)?;
    let start_at = opt_i64(p, "start_at")?.unwrap_or(now + recurrence);
    let slippage = opt_i64(p, "max_slippage_bps")?.unwrap_or(DEFAULT_DCA_SLIPPAGE_BPS);

    let intent = format!(
        "DCA step (PRE-AUTHORIZED): swap {amount} atomic units of {spend} into {buy}. Rules: \
         (1) read the {spend} balance first — if it is below {amount}, SKIP this run and report \
         'insufficient balance'; do not partial-fill. (2) slippage must be <= {slippage} bps; if \
         price impact exceeds 3% or no route is found, SKIP and report why. (3) on success, report \
         the filled amount, price, and cumulative bought to date. Never exceed {amount} atomic \
         units in a single run."
    );
    Ok(Rendered {
        scheduler: "schedule_cron",
        args: json!({
            "intent": intent,
            "trigger_at": start_at,
            "recurrence_seconds": recurrence,
            "application": recipe.app,
        }),
        app: recipe.app,
    })
}

fn render_idle_yield(recipe: &'static RecipeInfo, p: &Value, now: i64) -> Result<Rendered, String> {
    let reserve = req_str(p, "reserve_mint")?;
    let amount = req_str(p, "amount")?;
    let cadence = opt_str(p, "cadence").unwrap_or_else(|| "daily".into());
    let recurrence = cadence_seconds(&cadence)?;
    let start_at = opt_i64(p, "start_at")?.unwrap_or(now + recurrence);
    let min_apr = opt_f64(p, "min_apr")?.unwrap_or(0.0);
    let spacings = opt_i64(p, "range_spacings")?.unwrap_or(DEFAULT_YIELD_SPACINGS);

    let intent = format!(
        "Idle-yield sweep (PRE-AUTHORIZED): if the idle {reserve} balance is >= {amount} atomic \
         units, rank byreal CLMM pools by apr24h and pick the top pool whose apr24h is >= \
         {min_apr}% (if none qualifies, SKIP and report). Read that pool's tickSpacing and current \
         tick, then open a position with {amount} of {reserve} in a range of +/-{spacings} \
         tick-spacings around the current tick. Report the pool, tick range, and position value. \
         Never sweep below the reserve floor — only deploy funds above {amount}."
    );
    Ok(Rendered {
        scheduler: "schedule_cron",
        args: json!({
            "intent": intent,
            "trigger_at": start_at,
            "recurrence_seconds": recurrence,
            "application": recipe.app,
        }),
        app: recipe.app,
    })
}

enum Side {
    StopLoss,
    TakeProfit,
}

fn render_threshold(recipe: &'static RecipeInfo, p: &Value, side: Side) -> Result<Rendered, String> {
    let asset = req_str(p, "asset_mint")?;
    let quote = req_str(p, "quote_mint")?;
    let threshold = req_f64(p, "threshold_price")?;
    let poll = opt_i64(p, "poll_seconds")?.unwrap_or(DEFAULT_POLL_SECONDS);
    let expires_at = opt_i64(p, "expires_at")?;

    let (op, direction, label) = match side {
        Side::StopLoss => ("<=", "fell to your stop of", "stop-loss"),
        Side::TakeProfit => (">=", "rose to your target of", "take-profit"),
    };

    // Guard: byreal spot price for the asset vs the threshold. The read
    // returns `{ <mint>: "<price>" }` (envelope stripped), so the path is
    // the mint key itself.
    let condition = json!({
        "read": "byreal_spot_get_token_prices",
        "app": "byreal",
        "args": { "mints": [asset] },
        "path": asset,
        "op": op,
        "value": threshold,
    });
    let intent = format!(
        "{label} triggered (PRE-AUTHORIZED): the price of {asset} {direction} {threshold}. Sell \
         your entire {asset} balance into {quote} on byreal spot at market. Confirm the quote's \
         price impact is reasonable, execute, and report the fill (amount, price, resulting \
         {quote} balance)."
    );

    let mut args = json!({
        "intent": intent,
        "condition": condition,
        "poll_seconds": poll,
        "application": recipe.app,
    });
    if let Some(exp) = expires_at {
        args["expires_at"] = json!(exp);
    }
    Ok(Rendered {
        scheduler: "wake_on_condition",
        args,
        app: recipe.app,
    })
}

// ── Param helpers ───────────────────────────────────────────────────────────

fn req_str(p: &Value, k: &str) -> Result<String, String> {
    p.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("recipe param '{k}' is required (a non-empty string)"))
}

fn opt_str(p: &Value, k: &str) -> Option<String> {
    p.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn req_f64(p: &Value, k: &str) -> Result<f64, String> {
    match p.get(k) {
        Some(Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| format!("recipe param '{k}' is not a valid number")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("recipe param '{k}' must be a number")),
        _ => Err(format!("recipe param '{k}' is required (a number)")),
    }
}

fn opt_f64(p: &Value, k: &str) -> Result<Option<f64>, String> {
    match p.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => req_f64(p, k).map(Some),
    }
}

fn opt_i64(p: &Value, k: &str) -> Result<Option<i64>, String> {
    match p.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("recipe param '{k}' must be a whole number")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("recipe param '{k}' must be a whole number")),
        _ => Err(format!("recipe param '{k}' must be a whole number")),
    }
}

fn cadence_seconds(cadence: &str) -> Result<i64, String> {
    match cadence.trim().to_ascii_lowercase().as_str() {
        "hourly" => Ok(3_600),
        "daily" => Ok(86_400),
        "weekly" => Ok(604_800),
        other => Err(format!(
            "cadence must be hourly | daily | weekly, got '{other}'"
        )),
    }
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ===================================================================
// Tools
// ===================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ListStrategiesArgs {}

pub(crate) struct ListStrategies;

impl DynAomiTool for ListStrategies {
    type App = ByrealStrategiesApp;
    type Args = ListStrategiesArgs;
    const NAME: &'static str = "byreal_strategy_list";
    const DESCRIPTION: &'static str = "List the curated standing strategies (id, what they do, which venue app they run on, required params). Show this when the user asks what automations are available.";

    fn run(_app: &Self::App, _args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        let recipes: Vec<Value> = RECIPES
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "title": r.title,
                    "summary": r.summary,
                    "runs_on": r.app,
                    "params": params_help(r.id),
                })
            })
            .collect();
        Ok(json!({ "source": "byreal-strategies", "recipes": recipes }))
    }
}

fn params_help(id: &str) -> Value {
    match id {
        "dca" => json!({
            "spend_mint": "mint to sell (required)",
            "buy_mint": "mint to buy (required)",
            "amount": "atomic units per run (required)",
            "cadence": "hourly | daily (default) | weekly",
            "start_at": "unix secs of first run (default: one cadence from now)",
            "max_slippage_bps": "default 50"
        }),
        "idle-yield" => json!({
            "reserve_mint": "stablecoin mint to deploy (required)",
            "amount": "atomic units per sweep (required)",
            "cadence": "hourly | daily (default) | weekly",
            "min_apr": "skip if no pool clears this APR %, default 0",
            "range_spacings": "position width in tick-spacings, default 10"
        }),
        "stop-loss" | "take-profit" => json!({
            "asset_mint": "asset to watch + sell (required)",
            "quote_mint": "token to receive (required)",
            "threshold_price": "trigger price in USD (required)",
            "poll_seconds": "guard re-check cadence, default 60",
            "expires_at": "unix secs to stop watching (optional)"
        }),
        _ => Value::Null,
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct StartStrategyArgs {
    /// Which strategy: `dca`, `idle-yield`, `stop-loss`, or `take-profit`.
    pub recipe: String,
    /// Recipe params — see `byreal_strategy_list` for each recipe's fields.
    pub params: Value,
}

pub(crate) struct StartStrategy;

impl DynAomiTool for StartStrategy {
    type App = ByrealStrategiesApp;
    type Args = StartStrategyArgs;
    const NAME: &'static str = "byreal_strategy_start";
    const DESCRIPTION: &'static str = "Arm a curated standing strategy in one step: renders the strategy into a scheduled intent with guardrails baked in, then hands the host scheduler the exact call to persist it (schedule_cron for cadences, wake_on_condition for price triggers) targeting the right venue app. Confirm the strategy summary with the user BEFORE calling. This does not execute anything now and does not arm the wallet — after arming, remind the user to enable auto signing for the strategy's app so it runs unattended.";

    fn run_with_routes(
        _app: &Self::App,
        args: Self::Args,
        _ctx: DynToolCallCtx,
    ) -> Result<ToolReturn, String> {
        let recipe = find_recipe(args.recipe.trim()).ok_or_else(|| {
            let menu: Vec<&str> = RECIPES.iter().map(|r| r.id).collect();
            format!(
                "[byreal-strategies] unknown recipe '{}'. Available: {menu:?}",
                args.recipe
            )
        })?;
        let rendered = render(recipe, &args.params, now_unix_secs())
            .map_err(|e| format!("[byreal-strategies] {}: {e}", recipe.id))?;

        let preview = json!({
            "source": "byreal-strategies",
            "recipe": recipe.id,
            "title": recipe.title,
            "runs_on": rendered.app,
            "scheduler": rendered.scheduler,
            "rendered_args": rendered.args,
            "requires": {
                "app_grant": rendered.app,
                "signing_mode": "auto",
                "note": format!(
                    "To run unattended, the user must enable auto signing for '{}'. Until then each run pauses for approval.",
                    rendered.app
                ),
            },
        });

        let scheduler = rendered.scheduler;
        let sched_args = rendered.args.clone();
        ToolReturn::route(preview)
            .next(|next| {
                next.add_named(scheduler, sched_args).note(
                    "Persist the strategy NOW by calling this host scheduler tool with these \
                     exact args — the intent, timing/condition, and target application are \
                     fully rendered; do not edit them.",
                );
            })
            .try_build()
            .map_err(|e| format!("[byreal-strategies] route build failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn menu_lists_all_recipes_with_param_help() {
        for r in RECIPES {
            assert!(!params_help(r.id).is_null(), "no param help for {}", r.id);
        }
        assert!(find_recipe("nope").is_none());
    }

    #[test]
    fn dca_renders_schedule_cron_targeting_jupiter() {
        let r = render(
            find_recipe("dca").unwrap(),
            &json!({ "spend_mint": "USDC", "buy_mint": "SOL", "amount": "50000000" }),
            NOW,
        )
        .unwrap();
        assert_eq!(r.scheduler, "schedule_cron");
        assert_eq!(r.args["application"], "jupiter");
        assert_eq!(r.args["recurrence_seconds"], 86_400);
        assert_eq!(r.args["trigger_at"], NOW + 86_400);
        let intent = r.args["intent"].as_str().unwrap();
        assert!(intent.contains("PRE-AUTHORIZED"));
        assert!(intent.contains("SKIP this run"));
        assert!(intent.contains("50 bps"));
    }

    #[test]
    fn idle_yield_renders_schedule_cron_targeting_byreal_lp() {
        let r = render(
            find_recipe("idle-yield").unwrap(),
            &json!({ "reserve_mint": "USDC", "amount": "200000000", "min_apr": 8.5 }),
            NOW,
        )
        .unwrap();
        assert_eq!(r.scheduler, "schedule_cron");
        assert_eq!(r.args["application"], "byreal-lp");
        assert!(r.args["intent"].as_str().unwrap().contains("8.5%"));
        assert!(r.args["intent"].as_str().unwrap().contains("tickSpacing"));
    }

    #[test]
    fn stop_loss_renders_wake_on_condition_with_price_guard() {
        let r = render(
            find_recipe("stop-loss").unwrap(),
            &json!({ "asset_mint": "SOLMINT", "quote_mint": "USDC", "threshold_price": 180.0 }),
            NOW,
        )
        .unwrap();
        assert_eq!(r.scheduler, "wake_on_condition");
        assert_eq!(r.args["application"], "byreal");
        assert_eq!(r.args["poll_seconds"], 60);
        let cond = &r.args["condition"];
        assert_eq!(cond["read"], "byreal_spot_get_token_prices");
        assert_eq!(cond["op"], "<=");
        assert_eq!(cond["value"], 180.0);
        assert_eq!(cond["path"], "SOLMINT");
        assert!(r.args.get("expires_at").is_none()); // omitted when unset
    }

    #[test]
    fn take_profit_mirrors_upward_with_expiry() {
        let r = render(
            find_recipe("take-profit").unwrap(),
            &json!({
                "asset_mint": "SOLMINT", "quote_mint": "USDC",
                "threshold_price": "250", "poll_seconds": 30, "expires_at": 1_900_000_000
            }),
            NOW,
        )
        .unwrap();
        assert_eq!(r.args["condition"]["op"], ">=");
        assert_eq!(r.args["condition"]["value"], 250.0); // numeric string coerced
        assert_eq!(r.args["poll_seconds"], 30);
        assert_eq!(r.args["expires_at"], 1_900_000_000);
    }

    #[test]
    fn missing_params_and_bad_cadence_error_actionably() {
        let err = render(find_recipe("dca").unwrap(), &json!({ "spend_mint": "USDC" }), NOW)
            .unwrap_err();
        assert!(err.contains("buy_mint"), "{err}");
        let err = render(
            find_recipe("dca").unwrap(),
            &json!({ "spend_mint": "U", "buy_mint": "S", "amount": "1", "cadence": "fortnightly" }),
            NOW,
        )
        .unwrap_err();
        assert!(err.contains("cadence"), "{err}");
    }

    #[test]
    fn cadence_table() {
        assert_eq!(cadence_seconds("HOURLY").unwrap(), 3_600);
        assert_eq!(cadence_seconds("daily").unwrap(), 86_400);
        assert_eq!(cadence_seconds(" weekly ").unwrap(), 604_800);
        assert!(cadence_seconds("yearly").is_err());
    }
}
