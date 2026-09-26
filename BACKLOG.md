# Backlog

Forward-looking work only. What was *learned* lives in CLAUDE.md (and should
move to a findings log — see below); what was *done* lives in git. An item leaves
this file when it lands, and the commit says so.

Each item: what, why, and anything already known that shapes it.

## Direction: a vol-timing model, not a direction model (decided 2026-09-26)

The directional drift has no demonstrated edge: August's +0.0133 CRPS skill had a
95 % weekly-block interval of [−0.009, +0.031], came from AMD and MSTR (−0.0017 on
the ten symbols common to both months), and October scores −0.0095. What *is*
consistent across both months is about move **size relative to the option
market's price for it**: moves beat the ATM-IV-implied move when earnings fall
inside the horizon (+0.13 / +0.14 correlation), and index ETFs move less than
implied (realized ÷ implied 0.58–0.92). So the model's job becomes forecasting
**realized ÷ implied move** — when the market underprices a move, buy premium;
when it overprices one, sell defined-risk premium — with no view on direction.

The calc side now captures **several expiries per symbol every week**, which is
what this needs: the market's own event-implied move lives in the IV *difference*
between a near weekly and the monthly, and the earnings effect above is partly
our crude benchmark (monthly IV scaled to 7 days) rather than a mispricing.

- **Phase 0 — an honest yardstick.** Done: `model_eval` carries a weekly-block
  interval, the gate reads its lower bound, records live at
  `models/<label>/<dataset>.json`. No model passes on either month. Left open:
  the ranker gate has no interval yet (its Spearman is pooled over candidates,
  and holds at 0.38–0.56 on both months), and a rule for what "labeled" means
  across several datasets (every record passes? the latest?).
- **Phase 1 — the vol-timing model.** First cut done: `models/vol-timing.toml`
  (ATM IV × 0.85, × 2.0 when earnings fall inside the horizon) is positive on
  both months but inside noise; realized vol added nothing (CLAUDE.md, *The first
  vol-timing model*). Next inputs, as the weekly multi-expiry captures land:
  the **IV term structure** (near-over-far ATM IV) and the **event-implied move**
  it prices, which should replace the flat earnings multiplier — the monthly
  chain's IV is the wrong benchmark for a week with a release in it. Then the
  15-minute cadence (trend 20/60, RSI 28, ATR 28, `iv_median_lookback` 400,
  `bars_per_year` 6552). More weeks of data are what the interval needs most.
- **Phase 2 — direction-neutral structures.** Done (CLAUDE.md,
  *Direction-neutral structures*): straddles, strangles, iron condors and
  butterflies; crush-aware revaluation across a release; trades held through
  earnings counted apart; `strategies/vol_timing.toml` with no earnings guards.
  The structures follow the model; the flat ×2.0 earnings multiplier does not
  pay (15 earnings strangles, −$1 812).
- **Phase 3 — vol-aware exits.** Done: `close_after_earnings` closes a position
  held through a release on the first bar after it is priced; it helps long vol
  on both months and is in `exits/vol.toml`. Short-vol profit-taking showed no
  better setting than 50 % (CLAUDE.md, *Vol-aware exits*).
- **Replace the earnings multiplier with the market's event pricing** once weekly
  multi-expiry captures land: the event-implied move from near-over-far IV, and
  a post-event IV read from the chains after it instead of the trailing median.
- **Event data.** XOM has one event in the whole table (calc `events-sync` gap).
  Live trading needs a forward earnings calendar: EDGAR only records a release
  once filed, which a backtest reads in hindsight.
- The existing directional work (RSI gates, 14-day models, trend sweeps) stays
  in git and CLAUDE.md as history; re-check any of it against the Phase 0 gate
  before relying on it.

## Also next

- **The viz cannot see 15-minute chains**: `catalog.py`, `frames.BAR_SIZE` and
  `backtest.underlying_bars` all read `30min`. The report's new `bar_minutes`
  tells it which grid a run used.
- **calc's weekly script passes `--bar-size 30min`**, while 15 is now the grid;
  they should agree.
- **The validation scripts read a dataset's `expiry`** and fail on an
  `expiries` list.

## Record keeping for models, rankers and backtests (to examine)

The registry and the run outputs were designed for one dataset, and several gaps
show up the moment there are two:

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
  expiry over the same weeks is not independent evidence. (Aug-21 and Oct-16
  overlap 2026-07-20 → 08-20.)
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
