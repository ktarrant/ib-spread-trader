# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

A Rust backtester for **modular option-spread strategies**, built to validate
algorithms before they trade live through the IB API. The pipeline is three
composable stages:

```
Signal (RSI crosses 30)  ->  Structure (bull call spread)  ->  Selector (which strikes, what cost)
```

Each stage is a trait, and a strategy is a TOML file naming one of each. Live
execution is **not** in scope yet; the seams exist so it can be added without rework.

## Sibling repos — read, never modify

- **`~/ib-option-calc`** (Rust) — the capture tool that produced the snapshots.
  Its `CLAUDE.md` holds the IBKR constraints that will govern the live side
  (pacing, no historical greeks, expired options unavailable forever). Its parked
  **`algorithm-api`** branch has the right conceptual shape and a good
  `src/indicators.rs`, but its backtest engine prices from the `quotes` table
  (**empty** in real snapshots) and deep-clones state every bar. Read it; don't port it.
- **`~/ib-option-viz`** (Python) — the mature read side, validated against the same
  snapshot. `src/optionviz/pricing.py` is the reference for the option maths here,
  and its `CLAUDE.md` explains *why* the median parity fit beat least squares.
  Our pricing tests pin against it, so it is the arbiter when the two disagree.

## Commands

```sh
cargo build
cargo test
cargo run -- catalog            # what is backtestable
cargo run -- catalog --all      # including chains too thin to trade
```

Every subcommand prints **JSON to stdout and nothing else** (a convention shared
with `ib-option-calc`), so output pipes into `jq` and diffs cleanly between runs.

`--db` / `$IB_SNAPSHOT` picks the snapshot; the default is the newest `*.sqlite3`
in `~/Documents/Options Database`, which works because the filenames are date-stamped
and therefore sort chronologically.

## The snapshot is irreplaceable — read-only is enforced, not requested

Expired options cannot be re-fetched from IB at any price. `Snapshot::open` uses
the `file:...?mode=ro` URI *and* `SQLITE_OPEN_READ_ONLY`, and there is deliberately
no code path that opens a snapshot any other way.
`snapshot::tests::opened_snapshots_reject_writes` pins this — if that test ever
needs "fixing", something has gone badly wrong.

## A registered contract is not tradeable data

This is the single most important thing to internalise before writing a query.
The reference snapshot registers **4 280 option contracts and only 886 ever
received a bar**; two of its three expiries have none at all. Anything that plans
work by reading `contracts` will promise chains it cannot price.

`data/catalog.rs` therefore measures coverage from `bars`, and specifically from
its `quote` CTE: rows where **bid and ask both printed at the same timestamp**. A
lone bid cannot be filled against, so it does not count as coverage.

What `catalog` reports on the reference snapshot (2026-08-26), and what each
number is there to stop you assuming:

| | |
|---|---|
| 12 chains, **all expiry `2026-08-21`** | No expiry roll is testable. Build for it; validate on one chain. |
| ~2 months, 42–45 sessions, 543–619 timestamps | Thin. Per-trade logs mean more than Sharpe here. |
| `strikes` > `paired_strikes` on AAPL/AMZN/COIN/NVDA (36 vs 24 on AMZN) | The parity fit only sees paired strikes. |
| `stock_ts_overlap` < `timestamps` on SPY/QQQ/IWM only | Those options trade to 20:00 UTC, 15 min past the equity close. That bar has no spot — fall back to the parity forward. |
| 9 earnings across the window (TSLA has 2) | Event-driven signals are testable. |

## Conventions from the capture side that this repo must not re-learn

- **Timestamps are RFC3339 text with a literal `+00:00`.** Lexicographic `BETWEEN`
  works *only* because every writer emits that exact form. Parse and emit it exactly.
- **`ts` is the bar START.** 13 bars per session, 13:30–19:30 UTC.
- **`bars.volume` is 0 on every bid/ask row** — IB sends -1 and the column is
  `NOT NULL`. It is not "no trading", it is meaningless there. Never use it as a
  liquidity filter on those series; use `trades` bars or `option_snapshots.open_interest`.
- **`trades` is sparse, `bid`/`ask` dense.** Continuous series come from bid/ask;
  trades is an activity overlay only.
- **`bar_count = 0` in `capture_coverage` is a real answer**, not a gap — IB was
  asked and had nothing.
- **No historical greeks exist, ever.** IV and delta are derived here, not read.
- The `quotes` table is **empty** in real snapshots. Price from `bars`.

## Conventions

- `anyhow::Result` with `.with_context(...)` at I/O boundaries; `bail!` for
  user-facing validation.
- Logic lives in library modules under `src/`; `main.rs` is a thin clap dispatcher
  so everything stays unit-testable.
- Tests are inline `#[cfg(test)] mod tests` blocks.
