# Backlog

Forward-looking work only. What was *learned* lives in CLAUDE.md (and should
move to a findings log — see below); what was *done* lives in git. An item leaves
this file when it lands, and the commit says so.

Each item: what, why, and anything already known that shapes it.

## Next — when the October snapshot lands

- **Pin the golden tests to the reference snapshot by name**, separately from the
  CLI's "newest" default. Once a larger snapshot that includes October becomes
  the default, `the_catalog_reports_one_expiry_across_twelve_symbols` fails and
  the ranked goldens likely drift (more stock bars past expiry means more
  forecasts scored; the events table grows).
- **A dataset for October, with the new cohort.** `datasets/2026-08-21.toml` names
  AMD and MSTR, which calc no longer captures; XOM, GS, BA, UBER, NFLX are new,
  and calc's plan grows the cohort to ~36. Use `expiries = [...]` to trade
  several chains together.
- **Check the capture's option bar size.** calc's `--bar-size` still defaults to
  `15min`; the weekly script passes `30min`, but a manual/backfill run with the
  default produces chains this repo and the viz both ignore (both read `30min`).
  Fix belongs in calc (its default), or at least `catalog` should say when it
  sees option bars at another size.
- **Scripts do not understand `expiries`.** `validate_model.py` and
  `validate_ranker.py` read `expiry` with a regex and fail on a list;
  `sweep_model.py` hard-codes `dataset = "2026-08-21"`. Parse TOML with
  `tomllib` (stdlib) rather than regex while there. Default them to the release
  binary (a full backtest is ~12 s release).

## Record keeping for models, rankers and backtests (to examine)

The registry and the run outputs were designed for one dataset, and several gaps
show up the moment there are two:

- **Validation records are one per label** (`models/<label>.json`), so validating
  on October overwrites the August evidence — the exact out-of-period comparison
  the 14-day models were kept for. Records likely want to be keyed by
  (label, dataset), with the gate applied per record and a rule for what
  "labeled" means across several (all pass? the most recent?).
- **A backtest does not say what it ran on.** `backtest.json` has no snapshot path
  or date, no git commit, and `out/<name>/` is overwritten each run. With weekly
  snapshots, two runs of the same strategy can differ silently. Candidates:
  snapshot filename + schema version + commit in the report; runs written to
  `out/<name>/<date-or-hash>/`.
- **The viz reads the newest snapshot, not the run's** (`underlying_bars`), which
  the provenance field above would fix.
- **What "dataset" means is due a decision.** Today a label is a symbol list plus
  chains. With rolling weekly captures, a date window ("every chain expiring in
  Q4") may be the more natural unit, and out-of-period validation wants windows
  that do not overlap in time — weekly chains overlap heavily, so a second
  expiry over the same weeks is not independent evidence.
- Orphaned outputs: `out/mean_reversion_calibrated` has no strategy; four
  strategies have no `out/` run.

## Multi-expiry follow-ups

Multi-expiry trading is built (CLAUDE.md, *Several expiries*) but has only run on
fixtures. When real multi-chain data exists:

- **Validate it on real data** and add a golden for a multi-expiry run.
- **IV comparisons between expirations.** Now possible: every bar carries each
  chain's ladder. Ideas, roughly in order of cost:
  - log ATM IV per chain per bar (a term-structure table beside `forecasts.csv`);
  - a term-structure slope feature (near ATM IV over far) — the backwardation of
    an earnings week is the obvious first target;
  - interpolate ATM *variance* to the model horizon across the two chains
    bracketing it, instead of taking the nearest covering chain's IV;
  - calendar/diagonal spreads — legs in different expiries. `Spread` and
    `payoff_bounds` assume same-expiry legs, so this is a new structure, not a
    config.
- **`iv_vs_median` crosses chain switches.** When the feature chain changes (its
  expiry falls inside the horizon), ATM IV steps along the term structure and the
  trailing median sees the step as a regime change.
- **Per-symbol cap spans expiries.** `max_positions_per_symbol = 1` means one
  position per name across all its chains. Raising it allows one per chain,
  but the ranker still opens at most one candidate per symbol per bar.
- `ChainSeries::load` reads the underlying and earnings once *per chain*; fine at
  a few chains, worth sharing if load time shows up.

## Correctness and honesty in the report

- **Dropped-row counts (`Quality`) never reach `backtest.json`** — `SymbolSummary`
  has no field for them, though CLAUDE.md says the report prints them. And
  `ChainSeries.underlying_quality` reports the 5-min tape, dropping the 30-min.
- **`bars` counts 5-min stock-only ticks** (SPY: 4 878 against 581 option bars),
  which the ranked path skips; it grows with stock history, not the backtest.
- **The "no Sharpe" caveat is a hard-coded string** ("one expiry and ~44
  sessions"); count the expiries and sessions from the run instead.
- **TSLA's Item 2.02 filings include deliveries releases**, so they count as
  earnings for the blackout and the before-earnings exit. `period_of_report`
  equals the filing date there, so only cadence tells them apart.

## Viz (`~/ib-option-viz`)

- Show `expiries` (report) and each trade's `expiry` (new fields).
- "Horizon: 5d" on every run: it reads `ranking.horizon_days`, which moved to
  `model.horizon_days`.
- `forecast_stop` is missing from the exit-label map.
- The config panel hard-labels the ranker "E[P/L] ÷ max loss" whatever `metric`
  says, and never shows the labels, portfolio/sizing, RSI gates or
  `min_prob_profit`; it still renders retired staged-signal kinds.
- Test count in both CLAUDE.md files says 60; it is 66.

## Cleanup

- **Split CLAUDE.md** (1 475 lines): the dated research journal (~lines 403–1133,
  all on the Aug snapshot) into `FINDINGS.md`, leaving CLAUDE.md as guidance plus
  a short list of standing conclusions. Also out of date: the snapshot date
  (2026-08-26 vs 08-28), "the ranked engine is a second decision path, not a
  replacement", "a future RSI model would reuse `indicators::Rsi`", the variant
  table missing `mean_reversion_iv_sized*`; DESIGN.md says "four blocks".
- **Delete the staged pipeline** (~1 830 lines in `signal/`, `structure/`,
  `execution/selector.rs`, plus its branches in the engine and config) and the
  per-symbol `run_symbol` loop, which no shipped portfolio uses. Both are in git
  history. Blocker: the engine tests are built on the staged `bull_call_strategy`
  fixture and would need a ranked equivalent.
- `cargo fmt` (92 hunks of drift) — as its own commit so it does not bury a diff.
