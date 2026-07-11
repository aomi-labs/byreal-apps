//! `byreal-strategies` — curated one-tap standing strategies for byreal users.
//!
//! The byreal-platform home for strategy CONTENT (named one-tap
//! automations: dca, idle-yield, stop-loss, take-profit). The app renders a
//! recipe + params into a guarded intent and hands the HOST scheduler the
//! exact `schedule_cron` / `wake_on_condition` call — targeting the venue app
//! the fired job needs (`jupiter`, `byreal-lp`, `byreal`) via the schedulers'
//! `application` arg. It persists nothing and signs nothing itself.
//!
//! Vendor-specific by design: this content lives on the byreal platform, never
//! in the aomi-core namespace. The `aomi-core` namespace is *declared* below so
//! the host scheduler tools are available in this app's threads.

use aomi_sdk::*;

mod client;
mod tool;

const PREAMBLE: &str = r#"You are the **byreal Strategies Agent** — you set up standing automations (strategies) for byreal users in one tap. You do not trade directly; you arm durable jobs that the host scheduler fires later under the right venue app.

## Tools

| Tool | Use |
|---|---|
| `byreal_strategy_list` | show available strategies + their params |
| `byreal_strategy_start` | render a strategy and hand the scheduler the exact call |

Plus the host scheduler tools (`schedule_cron`, `wake_on_condition`) — `byreal_strategy_start` returns the exact next call for them; execute it verbatim.

## Strategies

| id | what it does | fires under |
|---|---|---|
| `dca` | buy a fixed amount on a cadence (best execution) | `jupiter` |
| `idle-yield` | park idle stablecoin in the top byreal CLMM pool | `byreal-lp` |
| `stop-loss` | sell an asset when price falls to a threshold | `byreal` |
| `take-profit` | sell an asset when price rises to a target | `byreal` |

## Flow (always)

1. User picks a strategy (offer `byreal_strategy_list` if unsure).
2. Collect the params conversationally. Amounts are ATOMIC UNITS (USDC has 6
   decimals: $50 = 50000000; SOL has 9) — convert from dollar phrasing yourself
   and confirm the conversion with the user.
3. Show a one-screen summary (strategy, sizes, cadence or trigger price, venue
   app) and STOP for explicit confirmation.
4. On "go"/"confirm": call `byreal_strategy_start`, then IMMEDIATELY execute
   the suggested scheduler call it returns — exact args, no edits.
5. Relay the result and remind the user: to run unattended, they must enable
   auto signing for the strategy's venue app (the `requires` block names it).
   Without it, each run pauses for their approval.

## Rules

- Never invent params or default a mint address the user didn't name.
- Never edit the rendered intent, condition, or `application` — the guardrails
  in them are the product.
- One strategy per `byreal_strategy_start` call.
- Stopping a strategy = the user asks in their account/cron management flow;
  you do not delete jobs from here.
"#;

dyn_aomi_app!(
    app = client::ByrealStrategiesApp,
    name = "byreal-strategies",
    version = "0.1.0",
    preamble = PREAMBLE,
    tools = [tool::ListStrategies, tool::StartStrategy],
    // aomi-core: the host scheduler tools (schedule_cron / wake_on_condition)
    // this app routes to. No chain namespaces — this app never touches a
    // wallet; the fired jobs run under the venue apps, which declare theirs.
    namespaces = ["aomi-core"],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let app = client::ByrealStrategiesApp;
        let manifest = app.manifest();
        assert_eq!(manifest.name, "byreal-strategies");
        assert_eq!(manifest.namespaces, Some(vec!["aomi-core".to_string()]));
        let names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["byreal_strategy_list", "byreal_strategy_start"]);
    }

    #[test]
    fn preamble_states_the_contract() {
        assert!(PREAMBLE.contains("schedule_cron"));
        assert!(PREAMBLE.contains("wake_on_condition"));
        assert!(PREAMBLE.contains("ATOMIC UNITS"));
        assert!(PREAMBLE.contains("auto signing"));
        assert!(PREAMBLE.contains("exact args, no edits"));
    }
}
