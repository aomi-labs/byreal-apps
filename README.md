# byreal-apps

byreal white-labeled Aomi platform apps — the partner-platform home for every
byreal-specific app, mirroring the role of
[`aomi-labs/somm-finance-apps`](https://github.com/aomi-labs/somm-finance-apps)
for Somm Finance. Vendor-specific surfaces live on the partner platform, not in
the official `aomi-sdk` bundle and never in the aomi-core namespace.

## Apps

| App | Tools | What it does |
|---|---|---|
| `byreal` | `byreal_spot_*`, `byreal_perps_*`, `byreal_lp_*` | The original monolith: spot swaps (AMM/RFQ), Hyperliquid perps, Copy-Farming analytics, reward claims |
| `byreal-lp` | `byreal_clmm_*` | CLMM position lifecycle: zap-in open/increase, zap-out, position/pool reads |
| `byreal-copy-trade` | `byreal_copy_*` | Copy Farming: leaderboard discovery + mirror a top LP's position |
| `byreal-strategies` | `byreal_strategy_*` | One-tap standing strategies (dca, idle-yield, stop-loss, take-profit) that arm the host scheduler |

The general-venue `jupiter` app stays in the official `aomi-sdk` bundle (like
`oneinch`/`zerox`).

## Layout & deploy

Hand-authored source layout: one crate per app under `apps/<name>/` with an
`aomi.toml` (`platform = "byreal"`). The hosted deploy flow
(`POST /api/platforms/byreal/deploy`, see product-mono
`docs/topics/platform-hosting/`) ingests source repos via the GitHub App and
materializes the somm-style `apps/<installation>/<revision>/<app>/` tree in the
platform repo — this repo is authored flat and is deployable as a source.

## Local development

Apps pin `aomi-sdk` by sibling path (`../../../aomi-sdk/sdk`) for local dev; the
deploy-time form is an exact published pin (e.g. `aomi-sdk = "=3.0.2"`), same as
somm-finance-apps.

```bash
# build + test one app
cargo test --manifest-path apps/byreal-lp/Cargo.toml

# load locally against the runtime (plugin dir form)
# see product-mono docs/topics/platform-hosting/facts/app-loading.md
```
