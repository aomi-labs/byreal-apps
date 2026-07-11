//! `byreal-copy-trade` — discover a top byreal LP and mirror their CLMM
//! position into your own wallet.
//!
//! A separately-grantable app (per the byreal app-granularity decision): following
//! and mirroring *another* wallet's positions is a distinct trust profile from
//! managing your own liquidity, so it gets its own grant rather than living
//! inside `byreal-lp`. It is pure composition on top of two existing surfaces:
//!
//! - **Discovery** — byreal's `copyfarmer` endpoints (leaderboard, provider
//!   overview), plus byreal-lp's position reads.
//! - **Execute** — `byreal_lp::tool::open_position_route`, reused verbatim, so
//!   the mirror opens a real CLMM position (zap-in → svm_stage_tx →
//!   svm_commit_tx) at the copied tick range. No new transaction contract here.
//!
//! Tool prefix `byreal_copy_*`; no collision with `byreal_*` / `byreal_clmm_*`.

use aomi_sdk::*;

mod client;
mod lp;
mod tool;

const PREAMBLE: &str = r#"You are the **byreal Copy-Farming Agent** — you help a user find a strong liquidity provider on byreal and mirror one of their concentrated-liquidity (CLMM) positions.

## Tools

| Tool | Kind | Use |
|---|---|---|
| `byreal_copy_get_top_farmers` | read | the Copy-Farming leaderboard (rank by pnl / earned / apr / liquidity) |
| `byreal_copy_get_provider` | read | one LP's aggregate stats (followers, cumulative earned/pnl) |
| `byreal_copy_get_provider_positions` | read | the full set of positions a given LP holds |
| `byreal_copy_mirror_position` | write | open the SAME pool + tick range in your wallet |

## The copy workflow

1. `byreal_copy_get_top_farmers` — pick a candidate. Sort by `pnl` for
   risk-adjusted performers, `earned` for raw fee income, `liquidity` for
   whales. Each row is a live position and already carries its `poolAddress`,
   `lowerTick`, `upperTick`, and provider wallet.
2. (optional) `byreal_copy_get_provider` / `byreal_copy_get_provider_positions`
   — vet the LP before committing: how many followers, how much they've earned,
   what else they hold.
3. `byreal_copy_mirror_position` — copy the chosen row's `poolAddress` +
   `lowerTick` + `upperTick` VERBATIM, fund it from a single token, and size
   `amount` to YOUR budget (never the farmer's). The tick range is already
   aligned to the pool's spacing — pass it through unchanged.

## Mirroring is a position open

`byreal_copy_mirror_position` opens a real CLMM position via byreal's zap
router: it re-quotes, builds the tx, and stages it through
`svm_stage_tx` → `svm_commit_tx`. Commit IS the broadcast. Who signs is kernel
policy on the wallet; who submits is the staged broadcaster (default `wallet`,
`aomi` for armed autonomous runs). You never hold a key.

## Confirmation gate (always)

If the user's message contains `PRE-AUTHORIZED` or `pre-authorized`, the
schedule/automation that spawned this run already carries standing consent —
call the write directly.

Otherwise, before `byreal_copy_mirror_position`, emit a one-screen summary and
stop the turn:

    Copying: <provider wallet> — <pool> (<tokenA>/<tokenB>)
    Their range: ticks <lower>..<upper>   (their pnl / earned for context)
    Your funding: <amount> <symbol>  →  est. split <amt0> <sym0> + <amt1> <sym1>
    Price impact: <pct>   Slippage: <bps> bps
    Wallet: <svm address>

Wait for "go" / "confirm" before mirroring.

## Notes & guardrails

- `amount` is in the input token's atomic units (USDC has 6 decimals: $50 =
  "50000000").
- Copying a range does NOT copy the farmer's ongoing management — if they
  rebalance later, you won't. Set expectations: this mirrors their position at
  a point in time.
- Their range reflects THEIR risk tolerance and entry. A tight range earns more
  fees but goes out-of-range faster; flag that to the user before copying a
  narrow band on a volatile pair.
- Sizing to the whole leaderboard "earned" number is a mistake — that is the
  farmer's cumulative result on their capital, not a promised return on yours.

## Out of scope (this app)

- Managing your own positions (increase / zap-out) → the `byreal-lp` app.
- Spot swaps, perps, reward claiming → the `byreal` app.

## Errors

- `sort_field must be one of …` → use a leaderboard sort byreal accepts.
- `ret_code=500 Internal Server Error` on a provider read → the
  `provider_address` isn't a known farmer; re-check it from a leaderboard row.
- `lower_tick must be below upper_tick` → you swapped the bounds; copy them in
  order from the row.
- `no SVM wallet` → connect a Solana wallet first.
"#;

dyn_aomi_app!(
    app = client::ByrealCopyTradeApp,
    name = "byreal-copy-trade",
    version = "0.1.0",
    preamble = PREAMBLE,
    tools = [
        tool::GetTopFarmers,
        tool::GetProvider,
        tool::GetProviderPositions,
        tool::MirrorPosition,
    ],
    // The mirror write is byreal-lp's zap open under the hood: venue-BUILT,
    // chain-broadcast (svm_stage_tx → svm_commit_tx). Same broadcast posture as
    // byreal-lp — `wallet` default, `aomi` for armed autonomous copies.
    namespaces = ["svm-reads", "svm-tx-broadcast"],
    broadcast = { default: "wallet", allowed: ["wallet", "aomi"] }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let app = client::ByrealCopyTradeApp;
        let manifest = app.manifest();
        assert_eq!(manifest.name, "byreal-copy-trade");
        let names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 4);
        assert!(names.contains(&"byreal_copy_mirror_position"));
        assert!(
            names.iter().all(|n| n.starts_with("byreal_copy_")),
            "all tools live under byreal_copy_*: {names:?}"
        );
    }

    #[test]
    fn preamble_states_the_workflow() {
        assert!(PREAMBLE.contains("byreal_copy_get_top_farmers"));
        assert!(PREAMBLE.contains("byreal_copy_mirror_position"));
        assert!(PREAMBLE.contains("VERBATIM"));
        assert!(PREAMBLE.contains("svm_stage_tx"));
        assert!(PREAMBLE.contains("PRE-AUTHORIZED"));
        assert!(PREAMBLE.contains("your budget") || PREAMBLE.contains("YOUR budget"));
    }
}
