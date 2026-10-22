# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

A Rust backtester for **modular option-spread strategies**, built to validate
algorithms before they trade live through the IB API. A backtest is a composition
of five labeled blocks, each a strict TOML file that can be validated and reused
on its own:

```
PriceModel  ->  Ranker  ->  PortfolioModel  ->  ExitModel        (over a DataSet)
 (forecast)     (score)      (size & fund)       (when to close)
```

Each block is a trait or a config, and a strategy TOML names one of each — so a
comparison changes exactly one label and the difference is attributable to that
axis. An earlier `Signal -> Structure -> Selector` pipeline is retired but kept
dormant behind `Strategy::Staged`. Live execution is **not** in scope yet; the
seams exist so it can be added without rework.

## Sibling repos

**`~/ib-option-calc` is read-only — except its headline scorer. `~/ib-option-viz` is writable.** The capture
tool owns data this repo can never regenerate, so nothing here touches it. The
viz is the read side for the reports this repo writes, and the two halves of a
change — a new field and the panel that renders it — belong together.

The one exception in the capture tool, agreed 2026-10-04: the headline scorer
(`news-score`: `src/events/score.rs`, `src/store/scores.rs`) lives there so it
runs after each sync on the capture box. Changing it (prompt, batching,
filters) is a change in that repo, on a branch. It opens the capture DB
read-only and writes only its own cache file; the rest of the repo stays
hands-off.

When changing the viz: it has its own `CLAUDE.md`, its own conventions and its own
golden tests (`uv run pytest`, 98 of them, skipping cleanly without a snapshot).
Run them. And note the dependency direction stays one-way: the viz reads this
repo's `out/` artifacts, so a field must exist in the report **before** the page
that renders it, and removing one breaks a consumer you now maintain.

- **`~/ib-option-calc`** (Rust) — the capture tool that produced the snapshots.
  Its `CLAUDE.md` holds the IBKR constraints that will govern the live side
  (pacing, no historical greeks, expired options unavailable forever). Its parked
  **`algorithm-api`** branch has the right conceptual shape and a good
  `src/indicators.rs`, but its backtest engine prices from the `quotes` table
  (**empty** in real snapshots) and deep-clones state every bar. Read it; don't port it.
- **`~/ib-option-viz`** (Python) — the mature read side, validated against the same
  snapshot. `src/optionviz/pricing.py` is the reference for the option maths here,
  and its `CLAUDE.md` explains *why* the median parity fit beat least squares.
  Our pricing tests pin against it, so it is the arbiter when the two disagree —
  being writable does not make it the place to fix a disagreement about the maths.

## Commands

```sh
cargo build
cargo test
cargo run -- catalog            # what is backtestable
cargo run -- catalog --all      # including chains too thin to trade
scripts/quick_check.sh          # ~2 min: does a change help, against IB? (below)
scripts/quick_check.sh vol_timing --label wider-stop   # one strategy, a named check
scripts/run_frontier.sh         # rerun the frontier into out/, archive the rest
scripts/run_frontier.sh --db D:/ib-research/research-YYYY-MM-DD.sqlite3   # on a research DB
scripts/run_frontier.sh --only vol_timing --db ...   # rerun one strategy's rows
python scripts/history_datasets.py <research DB>   # datasets/history.toml: every monthly, 2023-11 on
scripts/publish-results.sh      # out/ (with a bundle per run) to the server's LAN visualizer
```

Backtests run on the Windows PC (or the Mac), never on the capture server.

**The quick check is the loop for changes; the frontier is for publishing.**
A backtest is one core, and the PC has 16 threads, so both scripts run in
parallel. `quick_check.sh` runs the frontier strategies on all.toml's
monthlies (etf-all for the condor) on the IB capture alone and on the full
research DB at once, in ~2 minutes, into `out_check/<stamp>[-label]/`. Then
`seam_compare.py` prints, per strategy, the average trade (and count) on IB
alone, the full run inside IB's entry window, the full run whole and either
side of the 2026-06-17 seam, with the previous check's line under it. IB alone
is the reference: a change helps when the full run's in-window average moves
toward it without losing trades, and the IB column itself shouldn't drop.
Only the history (37 monthlies) answers whether a strategy works over three
years, and `run_frontier.sh` (4 jobs, longest first, ~15–20 minutes; `--only` for
the strategies a change touched) is for when a change is worth publishing.
The research DB that ib-option-calc's `scripts/refresh-research.sh` builds holds
IB's data plus UW's tape for 56 symbols back to 2023-11. The server only shows
the published results (ib-option-calc's `docs/server.md`, "The visualizer, and
publishing results").

Headlines are scored by an LLM on the capture server, not here:
ib-option-calc's `news-score` runs after each daily headline sync, scoring only
windows that sync closed, and `scripts/fetch_news_cache.sh` copies its cache
(`news-llm.sqlite3`) beside the snapshots, where `scripts/news_explore.py`
(through `scripts/news_scores.py`) and the viz read it, and a read-only dated
copy into `news/` there (a subfolder, so it never wins the default snapshot
pick). `scripts/news_audit.py` checks the cache and runs the event study below;
`scripts/news_study.py` runs the full study (*The news study*, below) into
`out/news_study.json`, which the viz's News Study page renders as the report.
`uv run scripts/ta_study.py` does the same for technical indicators on the
10-year stock tape (*The TA study*, below) into `out/ta_study.json`; it is the
one script with dependencies (numpy, pandas, declared inline for `uv`).

`out/` holds only the **frontier** — the runs listed in `scripts/run_frontier.sh`,
which is all the viz offers. Other runs are moved to `out_archive/`, not deleted.
The frontier runs `datasets/all.toml` (and `etf-all.toml`): every **monthly** chain
in one backtest. When a capture adds a month, add its expiry there; weeklies are
not traded yet and stay out by not being listed.

Every subcommand prints **JSON to stdout and nothing else** (a convention shared
with `ib-option-calc`), so output pipes into `jq` and diffs cleanly between runs.

`--db` / `$IB_SNAPSHOT` picks the snapshot; the default is the `*.sqlite3` in
`~/Documents/Options Database` whose filename **ends in the latest date**. Not a plain
name sort: the weekly capture writes `ib-option-calc-2026-10-01.sqlite3`, and the
older `ib-option-calc-snapshot-2026-08-28.sqlite3` would win that (`s` > `2`).

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
| 12 chains, **all expiry `2026-08-21`**, 30-minute bars | The 2026-09-26 capture adds 15 October chains at 15 minutes (see *Three cadences*). |
| ~2 months, 42–45 sessions, 543–619 timestamps | Thin. Per-trade logs mean more than Sharpe here. |
| `strikes` > `paired_strikes` on AAPL/AMZN/COIN/NVDA (36 vs 24 on AMZN) | The parity fit only sees paired strikes. |
| `stock_ts_overlap` < `timestamps` on SPY/QQQ/IWM only | Those options trade to 20:00 UTC, 15 min past the equity close. That bar has no spot — fall back to the parity forward. |
| 9 earnings across the window (TSLA has 2) | Event-driven signals are testable. |

## The option maths is derived, and validated against the Python

IB has no historical greeks, ever. So `pricing/` reconstructs them from the one
thing the capture does have — dense bid/ask across a whole strike ladder:

| Quantity | How |
|---|---|
| Forward | Median of `K + C - P` across paired strikes, two-pass with an ATM band |
| Implied vol | Black-76 inverted by bisection on that forward |
| Delta | Falls out of d1 once vol is known |

Black-76 rather than Black-Scholes because a **forward** is what we can recover;
the carry lives inside it, so the vol needs no rate or dividend assumption.
`discount_factor` is always 1.0 — a least-squares fit that tries to recover it
returns DF > 1 on 93% of real timestamps. Do not reintroduce it. The reasoning
lives in `pricing/parity.rs` and, at more length, in `~/ib-option-viz/CLAUDE.md`.

Every function is pinned to values generated by running
`~/ib-option-viz/src/optionviz/pricing.py` on the same inputs, to 1e-12. **That
module is the arbiter when the two disagree.**

Checked end to end on SPY at `2026-08-19T15:00:00+00:00` (30 paired strikes):

- parity forward **771.405** against a real stock mid of **771.12** — a +0.037%
  basis, the right sign and magnitude for carry
- residual MAD **0.020** (the viz repo reports 0.027 on SPY)
- call and put IV at the ATM strike, computed independently from different
  prices, agree at **0.1071 vs 0.1067**

**Deep-ITM calls legitimately return `None`.** At that same bar, strikes 719,
723, 748 and 751 quote at or below intrinsic, so no vol solves them. `implied_vol`
and `delta` return `Option` rather than NaN precisely so this cannot be defaulted
to zero — such a strike is *unusable*, and a selector must exclude it rather than
pick it. Debit verticals sit on the OTM side where the surface is clean (~0.10 IV
there versus a noisy 0.30–0.40 deep ITM), so this constrains structure choice, not
the common case.

## Load once, borrow forever

`ChainSeries::load` reads a whole symbol/expiry into one sorted `Vec` and every
per-bar view is a slice into it. This is not premature optimisation — it is
avoiding a specific mistake. The parked `algorithm-api` branch in
`~/ib-option-calc` rebuilt its history, quote map and contract map into a fresh
context on *every bar*, which is O(n²) over ~1M rows.

A full chain loads in ~100 ms (SPY: 581 bars, 48 660 quote rows). IV and delta
are resolved lazily in `ChainSeries::snapshot`, one pass per bar, so the engine
pays for exactly the bars it visits.

**Where a run's time goes** (`vol_timing` on all.toml, full research DB,
2026-10-21; measured with temporary timers): loading 18 s, the simulation 96 s.
Of that, resolving snapshots takes 36 s (mostly the 100-step bisection
`implied_vol`, kept because it is the Python reference's), choosing entries 53 s
(enumerating 21 s, the scorer evaluation 13 s, ranking ~20 s), and everything
else is under a second. Ranking was 204 s before `GridPricer`: each candidate
priced both its legs at all 101 grid points, and every candidate on a bar shares
those legs. The cache keys a leg by its exact strike, vol and right and sums the
same prices in the same order, so the outputs stayed byte-identical (all eight
quick-check runs, and `vol_timing_history`). Keep any further speed-up to that
bar: a faster IV solver or a GPU would change the numbers, and consumer GPUs
are slow at the f64 this maths needs.

## Losses are counted, never hidden

`Quality` tallies every row dropped on the way to a clean two-sided series —
unpaired, crossed, non-positive mid — and the run report prints it. Silently
discarding them would make a thin chain look like a clean one. On the reference
snapshot the option tape is good but not perfect: PLTR drops 389 unpaired rows
of 21 237 (1.8%), AMZN 235 of 27 295, SPY 11 crossed of 48 660. The underlying
tape is spotless (806 rows, nothing dropped, every symbol).

Note the asymmetry, which is deliberate: a defective *option* quote drops the
row, because an option that cannot be filled against is not tradeable. A
defective *underlying* quote only nulls `mid` and keeps the bar, because the
trades close still drives indicators.

## Fills cross the spread, in both directions

Opening a long leg pays the ask; opening a short leg receives the bid; closing
reverses both. Nothing fills at the midpoint, and mid pricing is not offered as
an option.

This is measurable rather than assumed only because the capture stores `bid` and
`ask` as *separate* bar series instead of a blended `bid_ask`. It matters more
than it looks: SPY calls average a 2.45% relative spread and a vertical crosses
it four times over a round trip. `an_instant_round_trip_loses_the_spread_not_zero`
pins the size of what mid pricing would have invented — on the test's quotes,
open-and-immediately-close loses $40, where mid-to-mid scores it at exactly zero.

Commission is per **leg** per contract (~$0.65), never per order. A 1-lot
vertical is ~$2.60 round trip, material against a $2-wide spread.

Two consequences to expect rather than debug:

- **Equity drops the instant a trade opens**, by the width of the book plus
  round-trip commission. A position entered at the ask and marked at the bid is
  genuinely worth less than it cost.
- **A leg the current bar does not quote is priced off that bar's smile**, not
  marked to zero or held at cost (see *Unquoted strikes* below). Only a bar with
  no smile at all falls back to entry cost for marks, and to intrinsic for a
  time-forced exit.

`Spread::payoff_bounds` derives max profit and max loss generically, by sampling
the strikes and both tails, rather than special-casing verticals — so a butterfly
or condor added later gets correct bounds for free. It returns `None` for a
structure with a runaway tail (a naked call, either side), because a profit
target expressed as a percentage of max profit is meaningless there. Naked
**puts** are bounded: the underlying cannot go below zero.

## Unquoted strikes: interpolated to enter, estimated to close

The captured strike set moves week to week — the tapered ladder in
`ib-option-calc` takes every strike out to ±0.5σ√T, one in two to ±1σ, one in
four to ±2σ — so a bar routinely lacks strikes a chain lists. On the 2026-10-03
snapshot a bar quotes a median **7 of 13** registered strikes within ±0.5σ√T of
the forward (p10: 4), and 13 of 24 within ±1σ√T. Near spot, the ladder the
ranker chose from was about half the real one.

`ChainSnapshot::estimate_quote` prices a strike off the bar's own OTM smile:
Black-76 at the smile's IV for that log-moneyness, crossed by the nearest quoted
Black-76 at an implied vol interpolated between the two nearest real quotes **of
the same right**, with the mid then held inside the bounds those two prices set
(calls fall and puts rise with strike, by no more than the strike step, and
never above the straight line between them), crossed by the wider neighbour's
spread. A strike no two same-right quotes bracket falls back to the OTM smile.

The first version read the OTM smile for every estimate, so a call below the
forward took the *put* side's vol. Where same-strike puts and calls disagree by a
vol point — a parity residual, not skew — that priced a SPY 739 call at 12.89
against a real 740 call at 12.965, and the ranker sold the real 737 against it
for a score of 1.41. Typical error was small; the ranker picks the tail. Entries
with an interpolated leg had scored higher than all-real ones (median 0.035 vs
0.027 on `mean_reversion`) and returned −4.2 % per trade against +7.2 %.

`examples/estimate_profile.rs` holds real quotes out (both rights of a strike, or
it and both neighbours) and measures the estimate, in units of the real
half-spread:

| held out | position | median | p90 | p99 |
|---|---|---|---|---|
| one strike | bracketed by the same right, \|z\| 0–2 | 0.14–0.18 | 0.50–0.74 | 1.4–2.3 |
| it and both neighbours | bracketed | 0.21–0.27 | 0.83–1.62 | 3.1–5.9 |
| one strike | past the same-right quotes (smile) | 0.46–2.56 | 3.5–61 | 15–142 |

The estimated spread runs ~1.1× the real one, by design. After the fix
`mean_reversion`'s interpolated entries number 21 of 79 (from 41), score like
real ones (0.031 vs 0.028), and return +13.2 % per trade; the run goes +11.8 % →
+21.9 %. So the rule splits on bracketing:

- **Entry**: `ChainSnapshot::interpolated` holds every registered strike the bar
  did not print but two real same-right quotes bracket, and `selectable` offers
  them beside the real quotes. Nothing past the quoted strikes is ever opened —
  META quoting only to +1 % still has no 0.30-delta call. A trade opened on one
  is `estimated_entry`.
- **Close and mark**: any unquoted leg, bracketed if possible, else off the smile
  (flat past the wings). Such a close is `estimated_price`. Both flags are
  counted in the caveats.
- **Features never see them.** Forward, ATM IV, skew and the smile itself read
  `quotes` only; `interpolated` is a separate field so no estimate can feed the
  surface it was drawn from.

Two consequences worth knowing. Every intrinsic exit on the reference snapshot
had been **flattering**: a TSLA call spread paid its full width a week before
expiry, a SPY credit spread called worth $0 because its strike was off an
odd-strike ladder (the smile prices the buy-back at ~$2.42). And "not quoted"
used to mean "not this chain" — `update_peaks` relied on another chain's bar
failing to price a position. An estimate does not fail, so every per-position
loop must filter on `(symbol, expiry)` itself.

On `mean_reversion` about half the entries take an interpolated leg — roughly
the share of interpolated strikes in the ladder, so the ranker does not favour
them. Interpolation does **not** pull the ranker toward the money: the median
trade still has a leg ~10 % from spot. `candidates.min_leg_delta` is that lever,
and **0.20 is the default for every ranker**: each leg's |delta| must sit in
0.20–0.80. The floor keeps entries on the near-money ladder every weekly capture
collects; the ceiling keeps a short leg from opening deep in the money, where
early assignment lives. `fixed_legs` (the skewed condor) are exempt — their
strikes are the rule. A ranker opts out with `min_leg_delta = 0.0`.

It was adopted on that structural argument, not on P&L. The P&L agreed, at 0.20
on the 2026-10-03 snapshot over every monthly chain: `mean_reversion` −$11 409 →
+$1 138 (legs >8 % out 50 → 25), `mean_reversion_prevailing` +$5 313 → +$7 237,
`vol_timing` −$1 226 → +$3 620 — three of three, on 16–94 trades. The ranker
records still pass with the band on; `credit_pop`'s Spearman falls most (0.45 →
0.38 August, 0.56 → 0.44 October), since its far-OTM shorts were the easiest
candidates to order and the band removes them.

## Expiry settlement is reported apart from traded P&L

The option tape stops the day *before* expiry — the reference snapshot's last
option bar is 2026-08-20 for a 2026-08-21 chain — so a position that hit neither
its target nor its stop cannot be closed on a quote. It settles at intrinsic
against the stock's own expiry-day print, which the capture does have.

`Portfolio::realized_split` keeps that separate from trades that closed on a real
quote, because settlement is the one part of a result the tape never priced. No
commission is charged to settle (IB does not bill exercise or assignment on US
equity options), which does mean a deep-ITM spread settles more cleanly here than
in life, where both legs assign and pin risk is real.

**The default expiry rules now avoid settlement entirely.** `min_days_before_expiry
= 3` and `min_entry_days_before_expiry = 8` are the config defaults for every
strategy, so a position always closes on a real quote before the tape stops rather
than settling at intrinsic — settlement in live trading is not something you want,
and with a single expiry a hard blackout is the honest stand-in. Once the capture
holds more than one expiry, the intended replacement is *rolling* to the next month
rather than blacking out. The settlement path stays because a config can still lift
the blackout, and its P&L is still reported apart.

**The entry blackout no longer has to cover the horizon.** A chain is scored to
the moment the before-expiry exit closes a position in it — `min(horizon,
time to expiry − min_days_before_expiry)` — so an entry nearer expiry than the
model's horizon is scored honestly rather than refused, and its drift is the
pro-rata share of the horizon's. `validate` now asks only that
`min_entry_days_before_expiry` exceed `min_days_before_expiry` (an entry inside
the exit would close on its next bar). Short-dated rules for weeklies are
`min_entry_days_before_expiry = 3` with `min_days_before_expiry = 2`, which closes
on the session before a Friday expiry — the last one the tape quotes. On the
defaults this re-scored the 9-day-out entries (6 days to their forced close, not
7): `mean_reversion`'s golden went 39 → 38 trades.

### Early assignment is flagged, never simulated

The backtest never assigns, and a short leg near expiry is where a real account
gets assigned. So every bar a short leg's **bid is at or below its intrinsic
value** — when its holder does better exercising than selling — is counted on
the position (`OpenPosition::early_assignment`: first bar, bar count), carried
to the trade, totalled (`early_assignment_trades`/`_pnl`), named in a caveat, and
badged in the viz trade card. A model-free read of the quotes; regular-session
bars with a stock print only (the 16:00–16:15 ETF option bar compares against
the parity forward or an after-hours print, and fired spuriously on a SPY call).
It cannot see **dividends** — the classic early exercise of a short call the day
before ex-date — because `underlying_snapshots.next_dividend_*` is empty in every
snapshot so far. On the 2026-10-06 snapshot the direction + width model with
every structure flags 2 of 115 trades, both deep-ITM short puts in their final
week (META 592.5, 14 bars; XOM 167.5, 2).

## Indicators are incremental, and warm-up is suppressed on purpose

Each indicator is fed one value per bar and carries its own state. The parked
`algorithm-api` branch rebuilt its indicator from the whole close history on
every bar — the same O(n²) mistake as its context cloning.

**The warm-up guard is not cosmetic.** `ta`'s RSI returns a value from the very
first `next()` call: 50.0 on bar one, computed from no price change at all. Left
unsuppressed, that reading alone fires a "crossed up through 30" trigger at the
start of every backtest. `Rsi` withholds readings until `period + 1` bars have
been seen (n changes need n+1 prices) and `Sma` until its window is full;
`the_unsuppressed_first_reading_would_have_been_a_false_signal` pins exactly what
that guard prevents.

`CrossDetector` remembers only the last **non-zero** sign, so a value sitting
exactly on the level is a touch rather than a crossing — otherwise one move
through the level fires twice, once arriving and once leaving.

Two behaviours that look like bugs the first time and are not:

- **The first comparison is a baseline, not a crossing.** A signal that starts
  mid-trend misses the crossing that trend began with, because the averages only
  become comparable once the slow one is warm.
- **Both averages must be advanced unconditionally.** An early `?` on the fast
  one starves the slow one, which then never warms and the signal fires nothing
  ever. Pinned by `nothing_fires_before_the_slow_average_is_warm`.

### Volatility-contraction breakouts use completed information only

`VolatilityContractionBreakout` composes four gates, all evaluated at a bar's
close. The contraction flag belongs to the **previous completed stock bar**; the
breakout bar is allowed to widen the bands without erasing the squeeze that
preceded it. Its historical percentile excludes the current measurement, and
the breakout high excludes the current bar. A run of consecutive new highs
fires only on its leading edge.

Underlying volume comes only from stock `trades` rows. Bid/ask volume is the
capture's meaningless zero sentinel. Intraday volume is strongly U-shaped, so a
bar is compared with the trailing median of the same New York 30-minute slot
across prior sessions rather than with adjacent bars.

ATM IV matches `~/ib-option-viz/src/optionviz/surface.py`: select the OTM side of
the smile and linearly interpolate implied vol at zero log-moneyness. A missing
stock high/close/volume or ATM IV fails the signal closed. The report's
`signal_diagnostics` counts each sequential gate so a sparse result can be
distinguished from a broken or still-warming signal.

A signal sees one bar at a time and keeps its own state, so it cannot reach back
into history it was not given — look-ahead is structurally impossible here rather
than merely avoided. `evaluate` must be called **once per bar, in order**;
skipping or replaying a bar silently corrupts every reading after it.

## Rejection is a first-class path, not an edge case

Resolving a structure against a real ladder is where a backtest most easily
starts lying to itself, because the honest answer is often "you cannot open this
trade". On the reference snapshot at 2026-07-15 15:00 UTC, SPY quotes 45 strikes
spanning −4.9% to +3.4% of spot — but **META quotes only to +1.0%**, so there is
no 0.30-delta call on its ladder at all. PLTR and QQQ carry ten strikes each.

A selector that quietly took the nearest available strike would turn a trade that
could not be placed into a trade that was, at a delta nobody asked for. So every
refusal is a typed `Reject` carrying **what was available** — "off by 0.02" and
"off by 0.28" are different findings — and the run report prints a census of them
beside the trade log. A strategy whose rejections outnumber its fills has not
been backtested; it has been imagined.

`Reject::DebitNotPositive` is worth knowing about: stale or crossed quotes can
make a debit spread price as a credit. That is not free money, it is bad data,
and accepting it would show up as edge.

Legs are named by **absolute delta**, which is what lets one `Vertical` struct
cover both the bull call and the bear put. For calls, strike rises as delta
falls, so long-0.50/short-0.30 is a bull call; for puts, strike rises as |delta|
rises, so the identical targets give a bear put. Only the right changes.

Only the **debit** version is built. A trigger disagreeing with the right's
natural bias (calls bullish, puts bearish) would make the credit version of the
same structure — the payoff maths already supports it — so such a trigger is
declined rather than inverted. Declines are counted separately from selector
rejections, because they mean different things: one is "not for this trigger",
the other is "the market could not supply it".

## The bar loop: decide on N, fill on N+1

A signal is computed from a bar's own close, and nobody can trade at a price they
are still waiting to observe — so a decision made on bar N executes against bar
N+1's quotes. `Timing::SameBar` exists only to measure what that costs.

**`SameBar` is not guaranteed to score better, and a test asserting it would be
asserting something untrue.** Look-ahead is an unfair advantage in expectation,
not on any single path: entering a bar earlier changes the entry price *and*
which bar the exit lands on, and the two interact. What is pinned instead is the
mechanism — the same signal opens the same trade exactly one bar apart, at a
different price.

Exits queue the same way. In life a profit target is a resting limit order and
would fill sooner, so next-bar exits are pessimistic — the right direction to err.

Order within a bar: execute yesterday's decisions, then mark, then evaluate
exits, then evaluate the signal. Exits are queued ahead of entries so a bar that
both closes and opens frees the slot first.

**A signal must be evaluated on every bar, in order, whether or not the result
can be acted on** — its indicators are stateful, and skipping bars when the
position cap is full would silently corrupt every later reading.

### Mixed resolution: 5-min signal, 30-min execution

`ChainSeries` loads two underlying timelines: 30-min bars contemporaneous with
option quotes, and 5-min bars (when the snapshot has them) for the signal. The
engine iterates the merged timeline — all timestamps from both sources. On option
bars (30-min), the full pipeline runs: fills, marks, exits, signal. On stock-only
bars (5-min intermediate ticks), only the signal advances.

A trigger that fires on a stock-only bar is carried to the next option bar, where
it is resolved against that bar's chain. This mirrors how a real trader would see
a stock move, decide to act, and check the option chain at the next available
snapshot. A trigger that fires on both a stock-only bar and the next option bar
uses the more recent one.

The 30-min underlying stays in `ChainSnapshot.underlying` for option bars so the
parity forward comparison against spot remains contemporaneous. Stock-only
snapshots use the 5-min bar and carry no option quotes — `atm_implied_vol()`
returns None and no strikes are selectable.

### Three cadences: 5-min watch, 15-min execution, 30-min forecast

Captures from 2026-09 on take **15-minute option bars** and 5-minute stock bars
(no 15- or 30-minute stock). Each chain reads its own bar size
(`ChainSeries::bar_minutes`, the finest the capture took); where native stock bars
do not reach the chain's last option bar, its underlying is **folded from the
5-minute tape** (first open, extreme high/low, last close, summed volume, the last
bar's mid). `catalog` lists each chain with its `bar_size`. A run whose chains
differ in bar size **folds the finer ones to the coarsest** (`ChainSeries::coarsen`)
and says so in a caveat — a 15- and a 30-minute bar starting together do not close
together, so they cannot share a timeline as they are. The 15-minute bar that
closes on a 30-minute boundary carries the same close quote, so it is kept and
relabelled; the rest are dropped. On the 2026-09-26 snapshot this is what lets the
30-minute August and 15-minute October chains run as one dataset, at the cost of
October filling 30 minutes after a decision rather than 15.

**A chain with both tapes merges per contract, not per timestamp.** The two
passes took different strike sets at the same instant — SPY's October 15-minute
band stopped at 730 where the 30-minute one reached 680 — and keeping the finer
bar's whole ladder dropped every strike only the coarser pass held. That hid the
skewed condor's 6.5 % SPY put on every overlapping bar, so SPY never traded it;
merged per contract, it takes 6 SPY trades and the run goes +1.4 % → +8.2 %
(28 trades). `RankingFunnel::fixed_unplaceable` now counts bars where a fixed
leg found no strike within tolerance, so a short ladder no longer reads as no
signal.

**Decided 2026-10-05: 15 minutes is the project's bar.** Options, the
underlying's execution bars and the headline-score windows (prompt v8) are all
15 minutes from now on, with 5-minute stock beneath them (back to 2016, TRADES
only). New work targets 15; nothing new is designed around 30. But 30 is
*legacy*, not *deletable*: expired options can never be re-fetched, so the
August chain and every 30-minute stretch already captured stay 30 forever, and
the fold, `coarsen` and the 08-28 goldens live as long as any dataset reads them.

What that means while most of the data is still 30:

- **Nothing executes at 15 yet.** On the 2026-10-05 snapshot 15-minute option
  bars exist only for the 10-16 chain (07-20 → 10-02) and SPCX; 10-16 also has a
  30-minute tape from 06-17, which the 15 does not span, so the loader folds it
  to 30 — and 11-20, 10-09 and August are 30-only. Every frontier run is a
  30-minute run. Execution reaches 15 only when a dataset's chains all start
  inside their 15-minute tape.
- **Every shipped model is a 30-minute model** (`cadence_minutes = 30`, periods
  in 30-minute steps, `bars_per_year = 3276` = 252 × 13), and every record under
  `models/` and `rankers/` was graded at 30. A 15-minute model is a new label
  (BACKLOG has the rescaled periods) validated on 15-minute data only — today
  ~10 weeks on one chain. Knobs counted in bars (`bars_held`,
  `max_bars_since_rsi_cross_up`, `prevailing_*`) change meaning with cadence;
  knobs in days and minutes do not.
- **The headline record restarted.** Readers take the newest prompt version
  only, so until the v8 re-score reaches 08-03 the windows before it read as
  unknown, and the news numbers in this file (v7, 30-minute windows) will not
  reproduce exactly.
- **The stock tape is no longer the short one.** Ten years of 5-minute bars
  fold to any cadence, so stock-only features can warm before the option tape
  starts and earnings-move sizes can be measured on ~40 releases a symbol — but
  anything needing IV is still bounded by the ~52 days of option history IB
  keeps.

Mixed tapes, as they load today: a chain with both keeps its finest tape when
that tape spans the coarser one; otherwise the finer tape is folded onto the
coarser grid and the coarser fills its gaps, so no captured week is dropped. The
30-minute pass also took a *different* strike set (IWM 250–310 against
265–305), so legs opened on the 15-minute ladder can go unquoted across the seam
and exit at intrinsic; the report counts those as synthetic exits. A
variable-cadence timeline (15 where captured, 30 elsewhere) was judged not worth
its cost while every dataset still includes a 30-minute chain.

- **Execution runs on every option bar**: fills, marks, P&L exits. A decision
  fills 15 minutes later instead of 30. `bars_held` counts option bars, so it
  doubles on a 15-minute tape for the same holding time.
- **The forecast runs at `features.cadence_minutes`** (default 30, what every
  shipped model was validated at). The feature engine advances only on the option
  bar that closes a cadence window, reading that **window's** stock bar
  (`ChainSeries::window_bar`) — an ATR over 15-minute ranges is a different
  indicator. Entries are decided only there; between, exits read the last
  forecast (`RankedContext` is keyed by expiry for that reason). On 30-minute data
  every bar closes a window, so August runs exactly as before. A 15-minute model
  is a new model label with rescaled periods and its own validation — expected
  once 15-minute captures accumulate.
- **`exit.stock_bar_exits`** (bundle `base_stock_watch`) watches the price-level
  exits — forecast target, adverse stop, short strike — on the 5-minute tape and
  queues the close for the position's next option bar. It barely fires with the
  horizon cap on (one extra target exit on August, −$6 on October's return), so
  it is off in `base`.

**The first out-of-period test failed.** On the October chain (15 symbols,
2026-07-20 → 09-25, 7 225 forecasts) `mean-reversion` scores CRPS skill
**−0.0095** and a 50.7 % hit-rate, positive on only **5 of 15** symbols — below
the gate's 10. The driftless null scores exactly 0.0000 with 53/83/93 coverage, so
the evaluation itself is sound on 15-minute data; the August edge did not carry.
PIT mean 0.55 again: the model sits below where prices landed — driven by the
high-beta single names, not the indices (see *The index put premium*).

### Several expiries: one forecast, a position per chain, never rolled

A dataset with `expiries = [...]` loads every chain of each symbol, and
`run_shared` takes `&[Vec<ChainSeries>]` — a symbol's chains, ascending by expiry.
Per bar and symbol:

- **Features are read once**, off the *feature chain*: the nearest expiry that
  still covers the model horizon, so its ATM IV is the vol of the period being
  forecast (the farthest when none covers it). Its expiry is logged on every
  forecast row. Switching chains as one expires steps ATM IV along the term
  structure, and `iv_vs_median`'s trailing window sees that step.
- **Each chain gets its own forecast**: horizon capped at *that* chain's forced
  close (expiry less `min_days_before_expiry`), drift pro rata to the capped
  horizon, and a sampled spot mapped through *that* chain's forward. A chain
  expiring **inside** the model's horizon forecasts with its **own** ATM IV
  (and its own expiry for `earnings_from_tape`), the market's vol for exactly
  the days it covers; any other chain uses the feature chain's. The scorer
  sample realizes each candidate at its chain's horizon (the last bar at or
  before it); `model_eval` always grades the forecaster at the model's full
  horizon, uncapped.
- **Entries go to the front month**: the nearest chain outside the entry
  blackout (`min_entry_days_before_expiry`), so a run trades August until it
  blacks out and then October. `execution.expiry_choice = "best"` instead ranks
  every open chain and takes the best candidate by the ranker's own key
  (`ranker::choice_key`). The funnel counts at most one `chosen` per bar either way.
- **A position lives in one chain.** `OpenPosition`/`ClosedTrade` carry `expiry`;
  marks, exits and closes read only that chain's bar. Read against another
  chain's bar a position looks unquoted and would be closed at intrinsic, so
  every filter is `(symbol, expiry)`. Nothing rolls.
- **Settlement happens mid-run**, at each chain's expiry-day print, freeing the
  capital for later entries. A chain whose tape never reaches expiry day (still
  live at the end of the capture) has no print, and its positions stay open,
  marked.

Several expiries need ranked mode and the shared account; `validate` refuses the
staged pipeline and the per-symbol loop, which each read one chain. With one
chain every step reduces to the old path — the full reports were diffed
byte-for-byte against the previous build, and the goldens pin it.

## The census is part of the result

`Census` records why bars produced no trade, and the distinctions are load-bearing:

| Field | Means |
|---|---|
| `declined` | The structure said "not for this trigger" — wrong direction for its right |
| `at_position_limit` | A position was already open |
| `rejections` | The market could not supply the trade, keyed by `Reject::kind()` |
| `unfillable_intents` | A queued order's legs were not quoted on the bar that arrived |

`rejection_examples` keeps one worked example per kind, so a census entry can
actually be read rather than just counted.

Exit rules read **gross** P&L, before commission. A profit target is a resting
limit order at a spread *price*, and no order can be placed that accounts for its
own commission. Commissions are charged in full and shown in the net P&L; they
are simply not what the trigger reads.

### The short-strike exit is a debit take-profit, not a credit stop

`underlying_past_short_strike` closes a position when the underlying reaches the
short strike. For a **debit** vertical that is near max profit — take it. For a
**credit** spread the short strike is the *loss* boundary, and a directional
credit bet can be past it **at entry** (a bear-call with the short call already
below spot), which would open and instantly close the trade for nothing but two
crossings of the book. So the rule fires for **debit structures only**
(`entry_debit > 0`); a naked long has no short leg and is unaffected either way.
`a_credit_spread_never_closes_on_the_short_strike` pins this.

Credit spreads instead run to the end of their **forecast window**:
`close_after_forecast_horizon` closes a position once it has been open as long as
`model.horizon_days` — the horizon it was scored to, after which the forecast no
longer speaks to it. It keys off the scoring horizon directly (so the two can
never drift apart) and applies to any structure, capping how long anything drifts
past the model's view. It sits below the take-profit and stop in priority, so a
winner or a real stop still fires first.

### Structure-scoped give-ups read the live forecast

The right *give-up* also depends on the structure, so three exits are scoped by
structure and read the **current** forecast at exit time (not the entry one):

- `live_pop_floor` — a **vertical**, debit or credit, gives up when its live
  probability of profit — the spread re-scored (`ranker::prob_profit_of`) against
  the current distribution with its sticky-strike entry vols — falls below the
  floor. The question is the same in both orientations (will this clear what it
  cost?), and it is the credit side's *only* loss-triggered exit.
- `naked_trailing_stop_pct` — a **naked long** closes when gross P&L gives back
  that fraction of its running peak (`OpenPosition::peak_value`, updated each bar
  in the mark step). Lets a winner run while capping the giveback.
- `naked_momentum_exit` — a **naked long** closes when the fast/slow trend flips
  against its direction.

Making these read the current forecast required one reorder: `ranked_observe`
(advance the feature engine, build the distribution) now runs **before** the exit
check instead of in the decide step, and hands an `ExitModel` (dist, remaining
years, forward scale, trend) to `compute_exits`; `ranked_choose` reuses the same
context. The reorder is behavior-neutral — the feature engine still advances once
per bar, in order — which the unchanged ranked goldens prove. `leg_vols` and
`peak_value` ride on `OpenPosition` to feed live POP and the trailing stop.

Exit is a framework axis: the `pop_trailing` and `momentum` labels under `exits/`
are selected by strategies that otherwise match the `mean_reversion` base, and
`scripts/compare_strategies.py`'s exit-mix column shows *how* each variant closes.

### The credit side had no give-up; the POP floor now covers it, and it did not help

`stop_loss_pct` and `underlying_past_short_strike` both require
`entry_debit > 0`, and the trailing and momentum stops require a naked long. So
until `live_pop_floor` was widened past `is_debit_spread`, a credit spread could
only close on a profit target, a forecast target, or the calendar: three ways to
win and none to cut a loss.

`credit_pop.toml` — credit-only, PoP-ordered — is what made that measurable, and
it is worth keeping for that. With `exits/base.toml` it takes 28 trades and
closes **7 on the profit target, 17 on the forecast horizon, 2 on the forecast
target, 2 before expiry: zero on risk**. It wins 16 of 28 and still loses 7.0 % —
average win $804 against average loss $1 189, worst trade −$2 538, a 5-lot
position at its full structural max loss that nothing could have closed sooner.

**The floor fires, does what a stop should, and still loses more.** With
`exits/pop_floor.toml` (`credit_pop_floor.toml`), 12 of 33 exits are the POP
floor, and the tail shrinks exactly as intended: worst trade −$2 538 → −$1 758,
average loss −$1 189 → −$973. The account nonetheless goes −7.0 % → **−22.6 %**
(profit factor 0.90 → 0.73), because the wins stay at 16 while the losses go
12 → 17: giving up frees capital that funds more entries, and a short-premium
position is given up precisely when the forecast has turned — which is also when
the time decay that pays it has the furthest left to run. The same widening cost
`exit_pop_trailing` its re-pinned golden (−$6 139 → −$8 571).

Read that as a guardrail, not a verdict: 28–76 trades on one expiry is noise, and
unlike a forecast an exit rule has no dense per-bar evaluation to appeal to. What
is *not* noise is that the entry was never the problem — the credit-only PoP
ranker discriminates best of the set (Spearman 0.447, monotone deciles, against
0.383 for `ev` and 0.395 for `prob_profit`).

**Sweeping the floor points the same way, and the entry gate is what worked.**

| run | trades | return | max DD | worst | POP exits |
|---|---|---|---|---|---|
| `credit_pop` (no floor) | 28 | −7.0 % | 42.0 % | −$2 538 | 0 |
| `credit_pop_floor10` (0.10) | 29 | −14.3 % | 46.8 % | −$3 123 | 6 |
| `credit_pop_floor15` (0.15) | 30 | −23.4 % | 45.1 % | −$2 083 | 8 |
| `credit_pop_floor` (0.25) | 33 | −22.6 % | 46.7 % | −$1 758 | 12 |
| `credit_pop_high` (70 % odds at entry) | 8 | −8.3 % | **17.9 %** | −$1 068 | 0 |

Loosening the floor recovers most of what 0.25 cost, but **no floor still beats
every floor tested** — on this dataset, giving up on a short-premium position is
a losing move at any threshold, because the decay that pays it needs the time
that giving up forfeits.

`min_prob_profit = 0.70` is the one that changes the risk. It is severe — 418 of
18 527 score-clearing candidates carry 70 % odds, 2.3 % — and it cuts max
drawdown from 42 % to **17.9 %** at a comparable return. It also makes the give-up
**redundant**: `credit_pop_high_floor10` is trade-for-trade *identical* to
`credit_pop_high`, because a position entered at 70 % odds never falls to 0.10
inside its horizon. Selecting better dominated exiting better.

Do not read the returns: 8 trades is not a sample. The drawdown and the
identical-runs fact are structural rather than P&L noise, and those are the parts
worth carrying forward.

### Trading both RSI extremes trades the model's worst regime half the time

`naked_rsi_trailing.toml` is the fun experiment, and it is three axes off the base
at once (nakeds only, RSI-extreme entries, trailing-stop exit) so it is read
against itself rather than against `mean_reversion`. `entry_rsi_zone = "outside"`
is the mirror of the band gate: it refuses the middle and acts only on a
stretched tape. Direction is not configured — the mean-reversion drift already
leans against the stretch, so the ranker buys calls when oversold and puts when
overbought on its own. **The gate decides when, the model decides which way.**

It works mechanically: 49 trades, all naked, 3 979 bars refused, entry RSI from
2 to 97, and 36 of 49 exits on the trailing stop. It also loses 53.9 %, and the
split says exactly why:

| entry regime | n | total P&L | wins | rights |
|---|---|---|---|---|
| oversold (RSI<30) | 17 | −$1 010 | 9/17 | **17C 0P** |
| **overbought (RSI>70)** | **32** | **−$9 772** | **8/32** | 1C **31P** |

Nine tenths of the loss comes from the overbought half — which the forecast eval
had already marked as the regime where this model scores **−0.0091 CRPS skill,
worse than a random walk**, against +0.0319 when oversold. Symmetric entries
against an asymmetric skill profile spend two thirds of their trades in the half
the model cannot forecast, buying puts into a rally that kept going.

Cutting to the good half is `naked_rsi_oversold.toml` — `max_entry_rsi = 30` with
the default `inside` zone, which already means "open at or below 30" and needed
no new knob:

| arm | trades | return | max DD | ret/dd | pf |
|---|---|---|---|---|---|
| both extremes | 49 | −53.9 % | 54.9 % | −0.98 | 0.23 |
| **oversold only** | 17 | **−5.1 %** | **16.3 %** | **−0.31** | **0.57** |

A tenth of the loss and a third of the drawdown, from removing entries rather
than improving any of them. Still negative — this is not a strategy — but the
gate does what the forecast eval said it would.

**Restricting to calls is a no-op, and that is the interesting part.**
`naked_rsi_bull.toml` adds `rights = ["call"]` and produces a *byte-identical*
run, because the oversold entries were **already 17 calls and no puts**. The
mean-reversion drift leans against the stretch on its own, so "trade the oversold
regime" and "be bullish" are the same instruction here — the direction never
needed configuring, only the timing did.

The lesson generalises past RSI: **a timing gate should be shaped like the
measured skill, not like the indicator.** "Both extremes" is a fact about RSI;
"oversold only" is a fact about this model.

### "Recently crossed up" beats "is oversold" — on one trade, and on 609 forecasts

`max_entry_rsi = 30` opens *while price is still falling*. `FeatureEngine` now
also counts `bars_since_rsi_cross_up` (a `CrossDetector` against
`rsi_cross_level`, default 30), and `execution.max_bars_since_rsi_cross_up`
refuses entries until RSI has actually turned back up — waiting for the bounce
rather than catching the knife. Never having crossed is a **refusal**, unlike a
missing RSI reading: "it has not turned up" is established, not unmeasured.

| arm | trades | return | max DD | ret/dd | pf | rights |
|---|---|---|---|---|---|---|
| both RSI extremes | 49 | −53.9 % | 54.9 % | −0.98 | 0.23 | 18C 31P |
| oversold, RSI≤30 | 17 | −5.1 % | 16.3 % | −0.31 | 0.57 | 17C 0P |
| **cross-up, ≤6 bars** | 24 | **+14.8 %** | 20.6 % | 0.72 | 2.03 | 24C 0P |
| **cross-up, that bar only** | 14 | **+18.2 %** | 15.1 % | **1.21** | 3.04 | 14C 0P |

**Do not read those returns.** One META trade is **+$3 444** — 116 % of the 6-bar
arm's total profit and 94 % of the tight arm's. Remove it and they are −$474 and
+$206: flat, not profitable. These are 14 and 24 trades, and they are the best of
roughly six gate variants tried on one snapshot, so the P&L is a selection
artifact until something denser agrees.

Something denser does, partly. Bucketing the 6 049-forecast log by distance from
a cross-up:

| bucket | n | CRPS skill | dir hit % |
|---|---|---|---|
| within 6 bars of a cross-up | 609 | **+0.0321** | 56.0 |
| 7–26 bars after | 964 | +0.0137 | 59.0 |
| 27+ bars after | 3 923 | +0.0032 | 54.6 |
| never crossed up yet | 553 | **+0.0612** | 63.8 |

Just after a turn the model forecasts ~10× better than deep into a stale trend,
which is a real gradient on real sample sizes and does support gating on
recency. But the **best bucket is "never crossed up yet"** — symbols still in
their first descent, which the gate refuses outright. That is a caution against
believing the mechanism as stated: what the data likes is not "the bounce
started" so much as "the trend is fresh", and the cross-up is one proxy for that
among several the gate does not test.

### `prob_profit` is calibrated where it does not matter and optimistic where it does

`ScorerRecord` carries `prob_profit` beside the realized outcome, so the
probability can be calibrated on the **dense counterfactual sample** rather than
on the trade log. That distinction is the whole answer: 79 trades put the miss at
−2.8 points, which reads like noise; 107 832 forward-priced candidates show it is
structural and concentrated.

| predicted | n | mean pred | actual | gap |
|---|---|---|---|---|
| 0–10 | 29 080 | 2.3 % | 3.4 % | +1.0 |
| 10–20 | 12 768 | 14.6 % | 17.5 % | +2.9 |
| 20–30 | 19 418 | 24.2 % | 28.6 % | +4.4 |
| 30–40 | 29 968 | 34.6 % | 36.3 % | +1.7 |
| 40–50 | 13 045 | 44.4 % | 41.7 % | −2.7 |
| **50–60** | 2 629 | 53.6 % | **42.8 %** | **−10.8** |
| **60–70** | 690 | 64.2 % | **43.6 %** | **−20.6** |
| **70–80** | 182 | 75.2 % | **52.7 %** | **−22.4** |
| 80–90 | 47 | 84.0 % | 72.3 % | −11.6 |
| *all* | *107 832* | *23.6 %* | *24.7 %* | *+1.1* |

Pooled it looks excellent — +1.1 points, and a Brier skill of **+0.112** against
always predicting the base rate, so it genuinely carries information. The pooled
number is also worthless for trading, because **every trade is drawn from the top
of the distribution**, and above 50 % the estimate is optimistic by 11 to 22
points. Two further facts pin the shape: the actual rate **flattens** at 42–44 %
across the 50–70 band, so in the region where candidates are chosen the estimate
has little discriminating power left as well as a bias; and the ordering below
50 % is monotone and near-honest, which is why the same number works as a
*ranker* and fails as a *probability*.

**This retro-explains the 70 %-odds gate.** `min_prob_profit = 0.70` selected
candidates the model called 77.3 % — realized **56.8 %** across the 234 such
candidates in the dense sample, and 3 winners from 8 in the run that used it. The
gate was not wrong to filter on odds; it was reading a number that is ~20 points
optimistic exactly where it was thresholding.

So: calibrate before gating on it, or gate on the empirical mapping rather than
the raw model number. And note the caveat in the measurement itself — the dense
sample's realized outcome closes at the forecast target or the horizon bar, which
matches `prob_profit`'s definition (clears cost at the horizon) closely but not
exactly.

### A trade records what the model concluded *and* what it saw

`prediction` on a trade has always carried the conclusion — `expected_pl`,
`prob_profit`, and `score`, which **is** expected P/L per unit of risk, the
ranker's own metric. `entry_features` now carries the other half: spot, trend,
VWAP distance, ATR, realized vol, ATM IV, skew, RSI and days-to-earnings, as read
on the bar the decision was made. Both land in `backtest.json` and as columns in
`predictions.csv`.

It rides the same path `ForecastBounds` does — `RankedContext` already holds the
features at the decision bar, `Intent::Open` carries them to the fill, and
`OpenPosition` to the close. Boxed inside the intent, because a diagnostic should
not set the size of every `Intent` the engine moves. Nothing reads it back: a
decision that consumed its own audit trail would be a look-ahead risk.

**An absent reading is omitted, never zeroed** — `skip_serializing_if` on the
struct, an empty cell in the CSV, "—" in the viz. A warming indicator reported as
`0.0` would read as a measurement, which is the same reason `implied_vol` returns
`Option`.

This is what makes the RSI gate auditable rather than merely configured: with
`max_entry_rsi` set, "what was RSI when this opened?" is now answerable from the
report instead of being the one thing it could not say.

### The model's skill is regime-dependent, and RSI finds the regime

The engine has no notion of *when* to act: it opens whenever a candidate clears
the floors and the symbol's one slot is free, so it is close to always in the
market — a position is open on **46.6 %** of option bars overall, and **79 % on
SPY, 77 % on QQQ**. There is no timing gate anywhere in the ranked path.

Conditioning the dense forecast log on RSI(14) of the 30-minute spot series shows
that is a mistake, on 6 049 forecasts rather than 51 trades:

| bucket | n | dir hit % | CRPS skill | mean abs. realized move |
|---|---|---|---|---|
| RSI <30 (oversold) | 470 | 53.8 | **+0.0319** | **5.82 %** |
| RSI 30–45 | 1 688 | 54.6 | +0.0210 | 5.42 % |
| RSI 45–55 | 1 587 | **58.0** | +0.0057 | 4.94 % |
| RSI 55–70 | 1 800 | 57.3 | +0.0124 | 4.41 % |
| RSI >70 (overbought) | 504 | 54.6 | **−0.0091** | 3.79 % |
| *all* | *6 049* | *56.3* | *+0.0136* | *4.89 %* |

The model forecasts **2.3× better than its own average when oversold and worse
than a random walk when overbought**, and the subsequent move shrinks
monotonically as RSI rises — so "ready to move" is literally measurable, and it
is the same axis. Per symbol, **RSI<45 gives positive skill on 9 of 12** — the
same robustness the shipped model itself clears — while RSI>70 manages only 5 of
12, with the failures enormous (PLTR −0.46, AMZN −0.29, SPY −0.20).

This is the same finding as three others in this file arriving from a fourth
direction. RSI>70 *is* the melt-up regime; the drift measurement said the model is
bearish on 64 % of bars, the PIT said it sits too low, and the sample rose. A
mean-reversion forecaster is broken in a rally, and RSI is a cheap, dense way to
name that state **before** entering rather than after losing.

Note the directional hit-rate points the other way — it peaks in the *neutral*
buckets. Direction and distribution disagree about where this model is good, so a
gate justified on CRPS should be stated as such and not sold as better direction.

**`execution.min_entry_rsi` / `max_entry_rsi` act on that**, refusing entries
outside the band and counting them as `census.rsi_out_of_band`. RSI rides on
`Features` beside the other indicators, advanced unconditionally so it warms; a
`None` reading refuses nothing, because the gate asserts "RSI is outside the
band" and a warming indicator has not established that. The price model ignores
the field — this is a gate, not a drift term. `mean_reversion_rsi.toml` sets
`max_entry_rsi = 70`:

| | trades | return | max DD | ret/dd | pf | debit | naked |
|---|---|---|---|---|---|---|---|
| base | 51 | −27.6 % | 54.8 % | −0.50 | 0.82 | +$2 023 | −$707 |
| `max_entry_rsi = 70` | 46 | **−5.0 %** | 41.0 % | **−0.12** | **0.96** | **+$5 512** | **+$320** |

874 bars refused. Five trades fewer turn −$5 510 into −$994, and both long
sleeves improve — the credit sleeve is untouched at −$6 827 because all five
credit entries happened below RSI 70.

**Treat this as regime-conditional, not as an edge.** The snapshot is a rising
market, where dips get bought and rallies keep running — exactly the conditions
that flatter a mean-reversion forecaster below RSI 45 and break it above 70. A
falling sample could invert the result and there is no second sample here to
check against, which is the same limit the 14-day model ran into. What the gate
is honestly built on is narrower than "RSI times entries well": it is "do not act
where this model has no measured skill."

**Correction, 2026-10-04: the skill gap did not survive more data, and the gate's
P&L is substitution.** On the 2026-10-03 snapshot over every monthly chain,
`mean_reversion`'s skill at RSI ≥ 70 is **+0.009** [−0.026, +0.044] (weekly
blocks), not negative; for `mean_reversion_prevailing` RSI ≥ 70 is the *best*
bucket of best-candidate realized P/L (+0.36 of risk on 38 bars). `max_entry_rsi
= 70` still improves both strategies in both the August and October/November
chains (`mean_reversion` 9.9 % → 32.0 %, prevailing 45.6 % → 62.7 %), but the
trades it removes were net −$552 and **+$2 651** respectively — the gain is the
27 and 7 trades the freed slots take instead. A gate that wins by what it lets
in rather than what it keeps out is path luck. It is not adopted.

The same pass measured the structure sleeves (`ev_no_credit`, `naked_only`):

| | return | max DD | ret/dd | Aug chain | Oct+Nov chains |
|---|---|---|---|---|---|
| `mean_reversion` (ev) | 9.9 % | 54.1 % | 0.18 | −$3 246 | +$6 184 |
| no credit | 12.2 % | 53.0 % | 0.23 | −$2 905 | +$6 321 |
| naked only | 19.1 % | **33.6 %** | 0.57 | +$1 744 | +$2 239 |
| `mean_reversion_prevailing` (ev) | 45.6 % | 44.0 % | 1.04 | −$5 405 | +$14 846 |
| no credit | 48.1 % | 42.6 % | 1.13 | −$4 923 | +$15 231 |
| naked only (12 trades) | 43.4 % | **13.0 %** | 3.34 | −$1 328 | +$10 013 |

Banning credit helps in all four cells, by $300–$500 each — but as path, not
edge (see below): a bearish credit spread comes back as a put debit spread. Naked-only is a
**risk profile, not an edge**: better in the losing period, worse in the winning
one, with drawdown cut by a third to two thirds. On the dense candidate sample no
structure separates (intervals ±0.3 of risk) — too few sampled candidates clear
the floors. What the losers share is how they end: 59 of `mean_reversion`'s 81
trades close at the forecast horizon for −$23 386, the 12 that reach the forecast
target make +$25 393. A loser is a trade that never moved.

**The credit-free policy was tried and reverted.** Every mean-reversion
strategy ran on `ev_no_credit` for a day; it is back on `ev` because the gain was
path, not edge (below), and because a higher score floor removes credit spreads
by itself — they are what the ranker picks on its weakest views, so they score
lowest. `ev_no_credit` remains as a label.

**A higher score floor does not help, on return or risk.** A conservative
prevailing variant (`ev` with a 0.06 floor) was built and retired the same day.
Its sweep first showed 0.06 cutting drawdown 44.0 % → 32.3 %, but that ran on
interpolated quotes priced off the other right's IV, and the higher floor
selected their mispricings (10 of its 14 trades had an interpolated leg). On
same-right bounded interpolation, over every monthly chain: floors 0.02 / 0.03 /
0.04 / 0.06 / 0.08 → 48.6 / 24.5 / 9.3 / 3.1 / 10.7 % return, 46.5 / 41.8 / 46.0
/ 46.4 / 10.3 % max drawdown (0.08 is 3 trades). The score orders candidates
within a bar but does not compare across bars, so a stricter floor trades less
without trading better. The general lesson: a filter that selects for high
scores also selects for whatever makes a score wrong.

And the debit sleeve is
not the steady earner it looks like — per dollar at risk, on the credit-free
runs:

| sleeve | n | win % | mean | median | avg win | avg loss | stdev |
|---|---|---|---|---|---|---|---|
| `mean_reversion` debit | 45 | 49 | −7.1 % | −1.1 % | +32 % | −44 % | 47 % |
| `mean_reversion` naked | 36 | 39 | +12.9 % | −17.3 % | +87 % | −34 % | 71 % |
| prevailing debit | 26 | 50 | −2.9 % | +0.1 % | +35 % | −41 % | 50 % |
| prevailing naked | 9 | 89 | +66.7 % | +68.1 % | +77 % | −12 % | 56 % |

Debits are tighter, and they win half the time with wins smaller than losses —
the cap takes the target exit's tail, and a timeout near the money still costs
most of the premium. Positive only on the Oct/Nov chains; nakeds positive on
both for `mean_reversion`. Naked calls have no `max_loss` (unbounded upside), so
their risk here is premium plus commission.

**Structure is a readout of how strong the view is, not a cause.** At the
decision bar, the forecast's drift toward the trade, `(q50 − spot) / stddev`, has
median +0.07 / +0.01 under the credit spreads the ranker picked, +0.16 / +0.13
under debits, +0.22 / +0.23 under nakeds (`mean_reversion` / prevailing): E[P/L]
per unit of max loss only favours a naked long when the expected move is large.
Swapping the wrapper on the *same* setup changes nothing — credit → debit on the
same symbol and bar −39 % → −43 % per unit of risk, same direction 3/3; debit →
naked −27 % → −34 %. The naked-only arm mostly does not take the debit setups at
all (42 of 45 have no same-bar counterpart): a naked cannot clear the score floor
on a weak view, so it trades less, and its result is *which* trades. The
credit-free gain is therefore path, like the RSI gate's; credit-free is kept for
its own reasons (no short premium, less assignment risk), not on that P&L.

The variable underneath has only a weak gradient on the dense log. By |drift_z|
on `mean_reversion`'s directional forecasts, the realized move toward the view
runs +0.06 → +0.14 sd and the hit rate 54 → 58 % from weakest to strongest; CRPS
skill peaks at 0.15–0.25 (+0.025 [−0.001, +0.045]) and turns negative above
0.25, where the drift overstates the move. Prevailing shows no gradient. So the
model's directional edge is a fraction of a standard deviation at any strength —
which is why a loser is a trade that never moved: most of each move is the part
the model does not see, and a weak view cannot pay a structure's spread and decay.

### ATM IV is already the whole vol input; what is missing is *elevation*

Worth knowing before proposing IV as a model input, because it is not merely an
input — with `realized_vol_weight = 0.0` in the shipped model, `annual_vol()`
returns `1.0 × atm_iv` and **ATM IV is 100 % of the forecast's volatility**. The
search that chose it is recorded above: IV-only calibrated best.

So the model already widens its distribution when IV rises into an event. What it
cannot do is tell *elevated* from *normally volatile*: it reads the IV **level**
and never the level relative to that symbol's own recent IV. `Features` carries
`days_to_earnings`, `days_since_earnings` and `residual_mad` (documented as "an
earnings-stress proxy") — and `drift()` and `annual_vol()` use **none of them**.
The earnings features are extracted for the engine's gates and ignored by the
model.

Measured on the snapshot's 9 in-window earnings events, ATM IV as a ratio to each
symbol's own median:

| | T−3 | T−1 | T+0 | T+1 | T+2 |
|---|---|---|---|---|---|
| META 07-29 | — | **1.27** | 1.27 | 0.93 | 0.91 |
| AMZN 07-30 | 1.12 | **1.17** | 1.25 | **0.81** | — |
| PLTR 08-03 | 1.20 | — | 1.29 | 0.94 | 0.85 |
| AMD 08-04 | — | 1.06 | 1.09 | 0.90 | 0.84 |
| COIN 07-30 | 1.08 | 1.09 | 1.06 | 0.91 | — |
| MSTR 07-30 | 0.99 | 0.98 | 0.97 | 0.92 | — |

Two things fall out. The **ramp** is small — T−1 against T−5 is a median 1.03× —
so a rate-of-change trigger has little to read; the **level** is what carries the
signal, up to 1.29× a symbol's own median, and MSTR shows it is not universal.
And the **crush dwarfs the ramp**: META 1.27 → 0.93 and AMZN 1.25 → 0.81 in a
single day.

**Which means the current earnings policy takes the wrong half of the cycle for a
credit seller.** `close_before_earnings = true` exits into the elevated IV and
`earnings_blackout_days = 3` refuses entries during the ramp, so a short-premium
position pays the run-up and never collects the collapse that pays for it.

One correction to the intuition that motivates this: on the runs on disk **no
credit spread has ever exited before earnings.** All 10 before-earnings exits were
long premium — 7 debit spreads and 3 naked longs, −$5 835 pooled — and for long
premium a rising IV is a *tailwind*, so those losses are directional, not vol.

The gap worth closing is therefore an **IV-elevation feature** (ATM IV over a
trailing median or percentile of its own history), which is what would let either
the model or an entry gate say "this is not normal volatility, it is a pending
event" — a distinction nothing in the pipeline can currently draw.

### A live forecast target is inert; the horizon cap is the real lever

**Do not rebuild `forecast_target` to read the current forecast.** It sounds like
the way to let a winner run and it is provably nothing: measured over every bar
of every open position in the base run, the live 0.85-quantile target sat beyond
current spot on **1108 of 1108** bullish position-bars. A distribution centred
near spot always projects more upside, so a target recomputed each bar retreats
exactly as fast as price approaches it. The rule would never fire — that is not
letting winners run, it is deleting the exit.

The live forecast *does* carry signal, in its **drift** rather than its quantile:
`q50 − spot` changes sign during **49 of 51** positions. But only **35.6 %** of
bars point upward at all (median drift −0.45), so a strict "close when the
forecast turns against me" rule would shut a bullish position almost at once. It
needs hysteresis to be a rule rather than a hair trigger — and that 35.6 % is the
PIT bias again: this model is bearish about two thirds of the time, in a sample
that rose.

**Switching the horizon cap off is the lever that works, and it is
structure-dependent.** `close_after_forecast_horizon` is 37 of the base run's 51
exits, so `exits/no_horizon.toml` is the largest change the exit axis allows
without new code:

| | trades | return | max DD | ret/dd | median bars held | avg win | avg loss |
|---|---|---|---|---|---|---|---|
| base (cap on) | 51 | −27.6 % | 54.8 % | −0.50 | 66 | $1 119 | −$1 039 |
| `no_horizon` | 26 | **−13.7 %** | **38.5 %** | **−0.35** | **144** | **$1 436** | −$1 426 |

Winners do run — the average win rises 28 % — and so do losers, which is the
honest half. But the split by structure is the finding:

| sleeve | cap on | cap off |
|---|---|---|
| naked longs | −$707 (12) | **+$3 932 (9)** |
| debit spreads | +$2 023 (34) | **−$6 666 (17)** |

A naked long has unbounded upside and wants the time; a **debit spread's payoff is
capped, so extra time adds no upside and all the downside** — and decay runs
against a long-premium position the whole while. The horizon cap should plausibly
be scoped like the give-ups already are: off for nakeds, on for spreads. That is
the same lesson the exit axis keeps producing, arriving from a third direction.

Read it with the usual caution: holding 2.2× longer means the one-per-symbol slot
frees up half as often, so `no_horizon` takes 26 trades rather than 51 and the two
runs are not the same trades measured twice.

### Banning credit spreads changes the wrapper, not the bet

`mean_reversion` takes 5 credit spreads and 4 of them lose, together −$6 827 —
*more than the account's entire* −$5 510. The obvious inference is that the credit
sleeve is the problem. It is not, and `mean_reversion_no_credit` (the `ev`
ranker with `allow_credit = false`) is what shows it: −27.6 % → **−26.9 %**,
recovering **$135** of a $5 510 loss.

43 of the 51 trades are byte-identical across the two runs and together make
**+$1 504**. The whole loss sits in the 8 that differ, and the substitutions are
the finding:

| dropped | | taken instead | |
|---|---|---|---|
| QQQ +5C 685 −5C 673 | −$2 758 | QQQ +5P 681 −5P 668 | −$1 848 |
| SPY +5C 751 −5C 742 | −$2 193 | SPY +4P 745 −4P 731 | −$1 634 |
| SPY +5C 779 −5C 770 | −$803 | SPY +5P 774 −5P 763 | −$2 068 |
| IWM +4C 299 −4C 290 | −$1 346 | IWM +5P 297 −5P 285 | −$848 |

Every bear-**call credit** spread comes back as a bear-**put debit** spread on
nearly the same strikes, because they are the same directional bet in two
wrappers — the symmetry `Vertical` is built on, arriving through the ranker
instead of the structure. Total: −$7 014 → −$6 879.

**So the structure was never the problem; the direction was.** All five credit
spreads were bearish, and they lost because the tape rose — which the forecast
eval had already said in a form nobody had connected to it: `pit_mean` 0.53 means
realized prices land above the forecast more often than they should, so the model
sits too low against a rising sample. A mean-reversion drift fades rallies, and
fading a rally that continues is a losing bet however it is expressed. Banning a
structure cannot fix a directional error; only the model can.

### A probability floor selects *against* payoff — measure reward/risk instead

The reason `credit_pop_high` disappoints is visible in what it picks. High odds
mean a short strike far out of the money, which is a small credit against a
full-width risk, so a probability floor is a filter on payoff with the sign
inverted:

| | median reward/risk | median width |
|---|---|---|
| `credit_pop` (no odds gate) | **0.92** | 9 |
| `credit_pop_high` (70 % odds) | **0.20** | 5 |

Reward/risk here is `max_profit / max_loss` — for a credit vertical, the credit
against the width it does not cover. 0.92 is a near-the-money short strike, a big
credit on close to a coin flip; 0.20 is the far wing. `min_reward_risk` /
`max_reward_risk` band the ratio directly (`credit_balanced`, 0.40–0.70), which
is the honest way to ask for "payoff near half the risk".

**The band does what it says and still loses more.** `credit_balanced` hits a 0.62
median ratio at width 7.5 — exactly the structure asked for — and returns −43.0 %
against `credit_pop`'s −7.0 %. Selection by structure is not selection by edge.

### The ladder is thin exactly where a balanced credit spread lives

Worth knowing before reading any of that as a verdict on the *idea*. At
2026-07-15 15:00, the strikes each symbol quotes in the 20–40 delta band — where a
~0.5 reward/risk short strike sits — are: SPY 16, IWM 12, META 7, QQQ 5,
AAPL/AMZN/NVDA/TSLA/COIN 4, **MSTR and PLTR 2**. Strike gaps are $1 on SPY and
IWM, $5–$10 everywhere else — 3.7 % of spot on PLTR.

So the band survives on only **7.4 %** of within-loss candidates, and on most
symbols the ranker is choosing among a handful of structures rather than tuning
one. A criterion this precise is partly a question the data cannot answer, and
that is a capture-side limit (see the `DataSet` note in DESIGN.md), not a
modelling one.

### The adverse-forecast stop is the first give-up that helps

`forecast_stop_quantile` closes a position when the underlying reaches the
quantile of the entry forecast *against* it — the mirror of the profit-side
target. Unlike `live_pop_floor` it reads a **price**, not a probability, so it
fires when the move the trade was underwritten against has actually happened
rather than when the position is merely behind and still has the time it needs.

| run | trades | return | worst trade | stops |
|---|---|---|---|---|
| `credit_balanced` | 20 | −43.0 % | −$2 328 | 0 |
| `credit_balanced_stop` | 27 | **−35.5 %** | **−$1 933** | 11 |
| `credit_pop` | 28 | −7.0 % | −$2 538 | 0 |
| `credit_pop_stop` | 35 | −9.7 % | **−$1 933** | 11 |

It cuts the tail every time it is used — the worst trade is the same −$1 933 in
both, the structural stop rather than a full-width loss. But it does not reliably
improve the account, because capital freed by a good exit is redeployed (28 → 35
trades on `credit_pop`). That pointed at the **portfolio** axis, and pulling on it
found the sharpest result in this whole line of work.

### Sizing by the IV regime: built, measured, and it does not work on the trades taken

`IvScaledSizer` sizes by ATM IV against its **trailing** median (the feature is
`iv_vs_median`, a rolling window read *before* the current bar is appended — the
full-sample median my analysis used is look-ahead and must never reach the
engine). Clamped 0.7–1.5, ignoring the score entirely, because the score is a
within-bar comparator.

Against a flat-one-lot control on the same strategy:

| portfolio | trades | return | max DD | lots | **$ per lot** | lot mix |
|---|---|---|---|---|---|---|
| flat 1 lot | 51 | −5.7 % | 14.1 % | 51 | **−22.37** | all 1 |
| IV-sized, $500 base | 47 | −7.3 % | 18.0 % | 57 | −25.69 | 42×1, 3×2, 1×4, 1×5 |
| IV-sized, $1 500 base | 51 | −21.2 % | 40.6 % | 153 | **−27.72** | 15×2, 27×3, 3×4, 6×5 |

The first arm did not test anything — 42 of 47 trades came out at one lot because
a $500 base budget buys exactly one lot of a candidate at the $500 loss cap, so a
1.5× multiplier cannot reach two. **Lot granularity, not the signal, decided the
size.** The second arm gives the regime real range and is *worse*, with P&L per
lot non-monotone in lot count (2 lots +$94, 3 lots −$97, 4 lots −$144, 5 lots
+$85).

**The reason is the trap this file already diagnosed for the score, walked into
again.** The +0.18 to +0.21 separation was measured over *all sampled candidates*.
Among the 41 trades actually taken it inverts:

| | n | mean IV ratio | total P&L |
|---|---|---|---|
| low-IV half | 20 | 0.84 | **+$69** |
| high-IV half | 21 | 1.04 | **−$2 387** |

A signal measured across the candidate population need not survive into the
per-bar argmax that becomes a trade — which is exactly what was said of
`scorer_eval`'s Spearman, and is just as true of any regime variable. **The
counterfactual sample is the wrong place to validate a sizing rule**, however
many observations it has; the right place is the selected set, and there 41
trades cannot support a conclusion either way.

So: the sizer stays, defaulted off, because the plumbing (a look-ahead-free
regime feature reaching the allocator) is the reusable part. The signal is not
adopted. What would settle it is more expiries, not more sampling.

### Realized-vs-implied vol splits long premium, not short — and it is an across-bar signal

The variance-risk-premium hunch — sell premium when implied exceeds realized, buy
it when realized exceeds implied — is testable now that `forecasts.csv` carries
the feature vector and `scorer.csv` carries `entry_debit` and `legs`. Joining them
gives 101 525 forward-priced candidates labelled by structure and by their bar's
vol regime, with no new backtest.

Realized P/L per unit of risk, by tercile of *annualised realized vol ÷ ATM IV*:

| structure | low (implied rich) | mid | high (realized rich) |
|---|---|---|---|
| credit | −0.532 | −0.528 | −0.526 |
| debit | −0.270 | −0.226 | **−0.196** |
| naked long | −0.064 | +0.040 | **+0.188** |

**Long premium behaves as the theory says; short premium does not.** Debits
improve monotonically as realized vol rises against implied (+0.074 of risk from
low to high) and naked longs — the purest long-vol structure — move +0.25. Credit
spreads are **flat**: 0.006 across the whole range.

**But the ratio is mostly the IV level showing through, and the level is the
better parameter.** Crossing the two for debits:

| | ratio low | ratio mid | ratio high | high−low |
|---|---|---|---|---|
| low IV (<0.97×) | −0.395 | −0.321 | −0.344 | +0.051 |
| mid IV | −0.232 | −0.183 | −0.106 | +0.127 |
| high IV (≥1.03×) | −0.184 | −0.148 | −0.171 | +0.014 |
| **IV effect within the band** | **+0.211** | **+0.173** | **+0.173** | |

Within a ratio band the IV level separates by a consistent +0.17 to +0.21. Within
an IV level the ratio separates by +0.05, +0.13, +0.01 — inconsistent and near
zero at both ends. **Control for IV and the variance-premium effect mostly
disappears; control for the ratio and the IV effect does not.**

So the parameter worth sizing on is **ATM IV against the symbol's own median**,
and unlike the ratio it works for *every* structure:

| structure | low IV | high IV | diff | symbols positive |
|---|---|---|---|---|
| debit | −0.356 | −0.174 | +0.182 | **10/12** |
| credit | −0.679 | −0.469 | **+0.210** | **11/12** |
| naked | −0.068 | +0.064 | +0.131 | 1/2 (n too small) |

11/12 and 10/12 are better per-symbol robustness than anything else measured
here, the model's own 9/12 included. And it is the same variable that predicted
*forecast* skill (IV 1.00–1.10× best, 10/12) — one regime variable moving both
the forecast and the trade, which is what a real state variable should do.

**Why this matters more than its size.** It is an *across-bar* discriminator,
which is exactly what the score lacks (within-bar Spearman +0.358, across-bar
+0.044). Scaling size by IV level injects the missing across-bar signal into
sizing rather than bolting on another binary gate — and the effect (+0.18 to
+0.21 of risk) is comparable to the RSI regime's +0.24, on 100 000 samples rather
than 24 trades.

### Why the score fails at sizing: it is a within-bar comparator, not an across-bar one

The score does its job, and the earlier "it does not size" finding is not a
contradiction — the two measurements are about different comparisons. On 107 832
sampled candidates:

**Within a bar it discriminates strongly.** Mean within-bar Spearman **+0.358**
(positive on 84 % of bars), against +0.383 pooled — so the pooled number is not
an artifact of ranking bars rather than candidates. Realized P/L per unit of risk
by within-bar rank:

| rank within the bar | n | realized/risk |
|---|---|---|
| top-ranked | 5 436 | **−0.040** |
| top 25 % | 26 746 | −0.059 |
| middle 50 % | 53 610 | −0.254 |
| bottom 25 % | 26 872 | **−0.835** |

Monotone, and the top is 21× better than the bottom quartile. **Choosing the best
candidate on a bar is where the ranker earns its keep.**

**Across bars it is nearly blind.** Take each bar's winner and ask whether its
score predicts its own outcome: Spearman **+0.044** over 5 436 bars, and the
quartile means are non-monotone. A score of +0.07 on one bar and −0.20 on another
say almost nothing about which trade to prefer — they are computed against
different distributions, so the scale is not comparable between bars.

That is exactly the comparison sizing makes ("this bar's winner deserves more
capital than that one's"), which is why score-weighted sizing measured worse per
lot than flat, and why the top half of *taken* trades underperformed the bottom
half — those trades come from different bars.

**And a regime variable separates the same winners 4.2× better than the score:**

| | n | winner's realized/risk |
|---|---|---|
| by score, highest quartile − lowest | | +0.057 |
| RSI < 30 | 362 | **+0.096** |
| RSI 30–45 | 1 456 | +0.007 |
| RSI 45–70 | 2 905 | −0.058 |
| RSI ≥ 70 | 460 | **−0.145** |

So the division of labour is empirical, not stylistic: **the score decides *what*
to trade on a bar; something else has to decide *whether* and *how much*.** The
gate is not a patch over a broken score — it is the across-bar mechanism the
score was never able to supply. Fixing the score would mean making it comparable
between bars (normalising against that bar's own candidate distribution is the
obvious first try, and is testable on `scorer.csv` without a backtest); until
then, sizing and gating should read a reliability signal rather than the score.

One thing this rules out: the optimizer's curse is *not* the explanation. The
top-ranked candidate beats its own forecast by 0.019 of risk while the
median-ranked beats it by 0.105 — the argmax is not selecting for estimation
error.

### The score orders candidates and does not size them

**`shared_20k` is nominally score-weighted and effectively flat at maximum
leverage.** `risk_per_position = 0.10` is $2 000, which already buys 4 lots of a
$500-max-loss candidate, so its *minimum* size is 4 and `max_lots = 5` clips the
top: its lots come out `{4, 5}` and nothing else. And `min_score` (the entry
floor) equals `baseline_score` (the sizing baseline) at 0.02, so the worst trade
taken gets multiplier exactly 1.0× and scores pile just above it — median 0.023,
2 of 28 ever reaching the 0.05 that would hit the risk cap.

`conviction_20k` sets `risk_per_position = 0.025` — one lot — so the multiplier
has somewhere to go. Run against `credit_pop_stop`, whose 35 trades are
**identical** in all three arms (same entries, same exits; only the lots differ):

| portfolio | mean lots | total lots | return | max DD | **P&L per lot** |
|---|---|---|---|---|---|
| `shared_20k` (4–5) | 4.66 | 163 | −9.7 % | 45.0 % | −$11.86 |
| `conviction_20k` (1–5 by score) | 1.57 | 55 | −10.0 % | 19.3 % | **−$36.45** |
| `equal_weight_1` (1 flat) | 1.00 | 35 | **−0.9 %** | 10.3 % | **−$5.03** |

**The more size is concentrated by score, the worse each lot does.** Sizing by the
score is not neutral here, it is actively harmful, and the direct check agrees: on
the flat arm, the bottom half of these trades by score averaged **+$83** and the
top half **−$88**.

That looks like it contradicts `scorer_eval`'s Spearman of +0.45, and it does not.
The Spearman measures the score across *sampled counterfactual candidates on every
bar* — a broad cross-section, most of which is never traded. The trades taken are
the per-bar argmax subject to the floors, so within that already-selected sliver
the score has no discriminating power left. **A score can rank a population well
and be useless or perverse among its own winners**, which is exactly the trap that
makes score-weighted sizing look obviously right. Rank with it; do not size with
it, on this evidence.

Read the returns as a guardrail (35 trades, one expiry), but note the mechanism is
supported twice over and independently — by P&L per lot across three sizing
policies on identical trades, and by the within-sample score/outcome split.

### Two weeks: the vol scales, the direction does not, and driftless starts trading

The horizon belongs to the model, so `driftless-14d` and `mean-reversion-14d` are
different forecasters and are evaluated at their own distance. A 2×2 over
{7d, 14d} × {mean-reversion, driftless}, holding the `credit_pop` ranker, the
adverse-forecast stop and flat sizing fixed — and holding
`min_entry_days_before_expiry = 14` on **all four**, including the 7-day arms,
because the 14-day horizon requires it and a shorter entry window would otherwise
masquerade as a horizon effect:

| arm | CRPS skill | dir % | coverage 50/80/90 | PIT mean | trades |
|---|---|---|---|---|---|
| 7d mean-reversion | **+0.0133** | 56.9 | 53/81/88 | 0.530 | 28 |
| 7d driftless | −0.0007 | 49.1 | 53/81/88 | 0.527 | **2** |
| 14d mean-reversion | +0.0051 | **51.6** | 54/82/90 | 0.562 | 23 |
| 14d driftless | +0.0003 | 46.1 | 54/82/91 | 0.560 | 11 |

Three findings, on 5 593–6 217 forecasts rather than on the trades:

- **The vol scaling holds to two weeks.** Coverage at 14d (54/82/90) is as good as
  at 7d and closer to nominal at the 90 % band. Nothing about extending the
  horizon breaks the distribution, which is the precondition for the idea.
- **The directional edge is gone, the distributional one is not quite.** 56.9 % →
  **51.6 %** is a coin flip, which is what "decays by two weeks" meant. But CRPS
  skill stays positive at +0.0051 — about 38 % of the 7-day value, and *not*
  validated under leave-one-symbol-out, so treat it as a hint that the drift still
  narrows the distribution usefully even where it cannot call direction.
- **A longer horizon is what makes a driftless model trade at all.** 2 trades at
  7d against 11 at 14d: with no directional view, almost nothing clears
  `min_score` until the distribution is wide enough to price a credit into. If a
  driftless credit book is the goal, the horizon is the knob that opens it.

**A caution the PIT surfaces.** `pit_mean` runs 0.53 at 7d and **0.56** at 14d, on
both models — realized prices land in the upper part of the forecast more often
than they should, so the forecast is centred slightly *low* and more so the
further out it reaches. The sample drifted up. For a credit book that is not
neutral: it over-rates bear-call spreads and under-rates bull-put spreads, and the
error grows with the horizon.

Trade P&L separates none of this (−4.2 % to −5.2 % across all four, 2–28 trades).

### Hunting a 14d model: momentum is still wrong, the SMA lookback is the knob

`scripts/sweep_model.py` sweeps model parameters and ranks them by forecast
quality alone. It needs no ranker, exit or portfolio — the eval does not depend on
them — and turns `scorer_sample_per_bar` off, which takes a run from ~5 minutes to
~35 seconds and makes a search affordable. Nineteen cells at 14d:

- **Momentum loses at two weeks too, monotonically.** `trend_weight` +0.25 →
  −0.0068, +0.5 → −0.0169, +1.0 → −0.0459, +2.0 → −0.1348, with the directional
  hit-rate pinned at 45 %. "Momentum needs weeks-to-months" was the standing
  explanation for v1's failure; at two weeks it is still wrong, and the best cell
  is still the mean-reversion **−0.5** the one-week model already uses.
- **VWAP adds nothing.** The best `vwap_reversion` beats zero by 0.0003 — noise.
- **The trend *lookback* is what matters.** `trend_slow = 50` beat `trend_slow =
  30` at every `trend_fast` tried (best 20/50 at **+0.0096**, against +0.0051 for
  the shipped 10/30). A gradient across the grid rather than one cell, and it fits
  the intuition that the horizon and the trend lookback should match.

**`mean-reversion-14d-slow` clears the gate, and the margin is one symbol.**

| | crps skill | dir % | PIT err | LOSO | verdict |
|---|---|---|---|---|---|
| `mean-reversion` (7d, shipped) | +0.0133 | 56.9 | 0.102 | 9/12 | passes |
| `mean-reversion-14d-slow` (20/50) | +0.0096 | **50.8** | 0.131 | **8/12** | **passes** |
| `mean-reversion-14d` (10/30) | +0.0051 | 51.6 | 0.120 | **7/12** | **fails** |

Every gate is cleared at its minimum: 8/12 is exactly `ceil(0.66 × 12)`, and the
directional hit-rate clears 0.5 by 0.008. The untuned 14d control fails on
**one symbol fewer**, so the entire difference between "labeled" and "not
labeled" is a single held-out symbol.

Two things that record does *not* say. It is **not better than the 7-day model** —
it is worse on skill, direction, calibration and robustness alike; it is the best
*14-day* model, not a better model. And it was found by a search over 19 cells on
the same snapshot that then validated it: LOSO guards against one or two symbols
carrying a result, not against a search fitting the period. With one snapshot and
one expiry there is no out-of-period test to appeal to, so treat the label as
provisional until the capture side supplies another expiry.


## Intraday levels: a stock-only day-trading path

`cargo run -- intraday [--config x.toml] [--trades]` (`src/intraday.rs`) trades
the **stock** on 5-minute bars, flat by the close: break of the initial balance
(first hour) or of the prior session's 70 % value area, then either a
**breakout** (close clearly beyond, a later close back within `retest_band` of
the level, target/stop in level-widths) or a **fade** (the excursion reaches
`fade_min_extension` widths before any retest, RSI crosses back through 70/30
while still beyond, target the level). Inside the range nothing trades. It shares
no code with the option engine and keeps its rules: decide on a close, fill at
the next bar's opening bid/ask, stops before targets when one bar reaches both,
$0.005/share with a $1 minimum.

Options are not the vehicle for this yet: the option tape is 15/30-minute and
monthly-only, and a typical move past the IB (0.34 widths, ~0.6 % of spot) is a
few crossings of an option's spread.

**First run, 2026-09-26 snapshot (17 symbols, 1 357 sessions): every variant
loses**, 4–18 bp a trade net of ~4 bp costs, across IB 30/60 min, value area,
targets, stops, bands, RSI levels and entry cutoffs. The by-day-type split says
why, and it is the user's own intuition measured:

| IB, defaults | back inside by close | beyond, <1 width | trend day, ≥1 width |
|---|---|---|---|
| breakout | 290, **−56 bp**, 2 % win | 267, +32 bp | 19, +103 bp |
| fade | 45, **+43 bp**, 82 % win | 149, −15 bp | 39, −47 bp |

`day_close_beyond` is **hindsight** — a label, never an input. Both setups
have edge on the right day type and give it all back on the wrong one, so the
strategy *is* the day-type call. The textbook tell does not make it:
`width_ratio` (today's IB over its trailing 20-session median) has narrow IBs
**worse** for breakouts (−14 bp) than normal ones (−8 bp).

### Context labels sort breakouts; the "sessions" claim does not survive the tape

Every trade carries entry-time labels, each read against its direction (`with`,
`against`, `neutral`, `unknown`): the open against the prior value area, SPY's
close against SPY's own levels, the 10:30–11:00 "reaction" move (known from
11:00), the 13:00–14:00 post-lunch move, and the entry window. `by_feature`
buckets them with **day-block** intervals (`model::eval::bootstrap_interval`,
shared with the CRPS gate) — 17 symbols on one day are one market, so ~80
days, not ~800 trades, is the sample.

Singly, every label moves breakouts the expected way and no interval separates.
Counting how many of the three (open, SPY, reaction) agree is monotone:

| breakout, labels `with` | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| trades | 94 | 234 | 196 | 52 (32 days) |
| avg net bp | **−30.7** | −9.9 | −2.6 | **+0.6** |

That is a day-type classifier starting to work, not an edge: the best bucket is
breakeven after costs. The count was built after the single-label table was
seen, though the favourable direction of each label was fixed beforehand.
Fades are not sorted by any label.

**A window's move does not predict the rest of the day.** `session_windows`
tests the expert claim directly on all 1 350 sessions: for every half hour (and
13:00–14:00), the share of sessions whose window-end-to-close move follows the
window's direction. All sit at 46–52 % with intervals spanning 50 %, 10:30–11:00
included (48.4 %). The reaction does not call the day; it only helps
**conditionally** — a breakout against it averages −17.7 bp, with it −3.3 bp.

Two timing rules do hold. **Power hour**: IB breakouts decided 15:00–15:45
(`last_entry = "15:45"`) average −19.5 bp [−27.8, −10.9], the one interval
clear of zero, and 15:00–15:30 is the one window whose move tends to *reverse*
(45.9 % [42.7, 49.3]). **Do not fade the reaction**: fades decided 10:30–11:00
average −55.6 bp (IB, 14) and −20.7 bp (value area, 167). The first hour, which
only value-area levels can trade, is *not* worse than the rest (breakout
+3.1 bp, fade −3.9 bp).

### Gating on context improves breakouts and does not make them pay

`breakout_min_context = N` refuses a breakout with fewer than N of the three
labels `with` it (`census.context_refused`). Split at 2026-07-27:

| breakouts | all | May–Jul | Jul–Sep |
|---|---|---|---|
| no gate | 576, −9.9 bp | −3.0 | −16.8 |
| ≥1 label | 482, −5.8 bp | +1.1 | −12.8 |
| ≥2 labels | 248, −1.9 bp | +4.3 | −8.9 |

The gradient holds in both halves; the sign does not. Dropping the target and
riding strong-context breakouts to the close (tried at ≥1, ≥2, ≥3) never beat
the fixed target, so it was removed. There is no profitable intraday model
here on this data — what carried over is the execution work below.

## Execution: crossing the spread is not the pessimistic assumption it looks

`cargo run --release --example execution_profile` measures, over every
tradeable chain and every quote at |delta| 0.15–0.65 (≈3 M limit-order
simulations on the 2026-09-26 snapshot), what the time of day and a passive
order would change about a fill. Slots are **quote** times — a bar's close,
`ts` plus its size — because that is the price the engine fills at.

**Spreads are widest at the open.** The median relative spread is 3.2–3.7 % in
the first half hour against 2.1–2.2 % from 10:30 on (15-minute chains; 3.2 %
against ~1.9 % at 30 minutes), and widens again on the 16:00 quote (3.0 %).
**ATM IV is richest at the open**: +0.7–1.0 % of its session mean, drifting
to −0.2–0.3 % after 13:00 — a morning premium roughly half a spread in size.

**A resting limit at mid does not beat crossing.** It fills 50–87 % of the time
within 1–4 bars, and saves −0.23 % to +0.08 % of mid against crossing at once
once the unfilled orders chase. Fills pocket half the spread; the misses are
orders the market moved away from, and chasing them costs as much. That is
adverse selection, measured before counting what the filled positions then do.
So the engine's "pay the ask, receive the bid" is close to what a naive
passive policy would achieve, not a pessimistic bound on it. The exception is
the open: resting at mid in the first half hour saves 0.3–0.8 % of mid (both
sides), because that is where the spread is abnormally wide. Resting late
(15:00–15:30) costs 0.5–0.6 %, chasing into the wide closing quote.

**Deferring fills past the open was built, measured, and removed.** An
`execution.fill_after_open_minutes` rule held queued orders until a later
quote. On mean_reversion, credit_pop and vol_timing over both datasets:
45 minutes was a wash (−$85 across six runs), 75 minutes +$1 280 — but only 31
fills changed, 12 of the 22 long-premium exits improved, the median change was
+$7, and two trades (COIN +$975, PLTR +$590) were the whole total. Waiting
45–75 minutes exposes a position to that much underlying movement, which
swamps a spread saving of ~1 % of premium. One trap worth knowing if it is
rebuilt: the engine queues an entry on every bar even when the symbol's slot
is full (the fill-time cap refuses it), so a deferred queue must let the
deferring bar's fresh decision supersede its entries — otherwise an entry
decided before an earnings release fills after it.

What this leaves for the live side: cross, or rest at mid only when trading in
the first half hour. Execution will not rescue a strategy that loses crossing.

## LLM headline scores: clean labels, no forward edge

The 2026-10-04 cache (prompt v7, 594 calls, 4 727 stories) covers 08-03 → 10-02
in one segment. It was drained oldest-first, then newest-first, and for a day
held two segments with a month between; `news_scores.segments` splits on a gap
over four days, and every reader treats a time outside a segment as
**unknown, not quiet**. About one window a day has no call — all of its
headlines were dropped by the noise filter — and 98 % of headlines sit in a
scored window.

The answers are structurally clean: every item answered, no stray ids, nothing
out of range, no item later than its window, every future event dated after
"now". Materiality 0/1/2/3 is 31/46/17/6 %, 43 % are repeats, and a hand sample
of the 3s is real catalysts (guidance, FAA 737 inspections, M&A). Most fresh
material news lands in the overnight window, so the first price to see it is
the 09:30 open.

`news_audit.py`'s event study (5-minute stock tape, SPY-adjusted single names,
day-block intervals, 43 days, 684 fresh material events ex-releases):

- **The scores describe the move already made.** In the LLM's direction the
  window's own return is +0.10 sd [+0.04, +0.16], +0.17 overnight.
- **Nothing forward.** Next close +0.07 [−0.02, +0.16], fifth −0.02, hit-rate
  52 %. Sentiment the same.
- **A same-day vol flag, and calm after.** Variance against quiet bars at the
  same entry time: `vol` elevated/high runs 1.31× [1.02, 1.66] for the rest of
  the session, materiality 2 1.33× — then 0.81× at the next close and 0.71×
  at the fifth for all fresh news. The move happens with the news and the
  following days are quieter than a quiet bar's, not louder.

**The record behind those numbers has holes.** IB returns only the newest 255
headlines per provider and symbol, so a heavy name is complete only from
`news_profile.complete_from` (NVDA 09-24, META 09-23, AAPL 09-10, AMZN 09-09,
TSLA 08-24), and NVDA had no headline scored in the first three weeks of
August. The audit reads those weeks as quiet. Inside complete coverage the
window move holds (+0.12 sd) and so does the calm fifth close (0.76×), but the
same-session vol flag falls to 1.21 [0.98, 1.42], within noise. A reader that
counts "no news" must mask by `complete_from`; the audit and the engine's
sentiment gate do not yet (BACKLOG).

**`future_events` is not a forward calendar, and the snapshot already has
one.** Its 17 earnings mentions are nearly all same-day or same-week (BA's
10-27 from 09-30 is the one with lead time), and NVDA's 08-26 print was never
announced. The `events` table carries `fomc`, `cpi`, `ppi`, `jobs_report`,
`gdp`, `pce` and `retail_sales` with release times, 31 releases from 05-27 to
10-02.

**A decayed sentiment indicator** (`news_scores.sentiment_index`, drawn as a
pane in `ib-option-viz`): materiality × sentiment summed with a
2^(−age/half-life) decay, beside the same sum of materiality (*intensity*);
tone is their ratio. The LLM runs positive (+0.15 mean, 2:1), so read tone in
terciles across rows, not against zero. Tone tracks the move since the prior
close (low tercile −0.11 sd, high +0.09 to +0.14) and orders nothing after it.

**Both hypotheses fixed on the half-scored cache failed on the full one.**
"A move the 48 h tone opposes reverses" went −0.38 [−0.68, −0.12] → −0.02
[−0.18, +0.14] over five sessions. "Mean-reversion fades the LLM agrees with
hit more often" went 64 % → 52 %, against 55 % when it disagrees (weekly
blocks, 8 weeks of `forecasts.csv`). Skipping entries with fresh news does not
help either: no-news bars forecast no better, and the trades taken within a day
of materiality-2+ news made +$5 847 over 17 against −$2 298 over 18 with none —
a few winners, not a reason to prefer news. Two weeks of data had produced two
convincing-looking cuts; seven weeks removed both. The one cell left standing
(`mean_reversion_prevailing` CRPS skill +0.011 [+0.005, +0.029] when the news
agrees with its fade, hit rate unchanged) is one of ~16 and is not adopted.

**Gating the prevailing fade on sentiment's sign costs half the return.**
`execution.sentiment_gate_half_life_hours` (`data::NewsSentiment`, the same
reduction as the Python and the viz) vetoes a ranked choice whose direction
disagrees with the sign of the decayed sentiment — the model still chooses,
the news can only refuse; unknown sentiment and neutral structures pass, so
the run matches its base until the cache is scored and warm (08-09).
`mean_reversion_prevailing_sentiment` over every monthly chain, 48 h:

| | trades | P&L | return | max DD | ret/dd | pf |
|---|---|---|---|---|---|---|
| `mean_reversion_prevailing` | 32 | +$9 928 | 49.6 % | 46.5 % | 1.07 | 1.86 |
| + sentiment gate | 21 | +$4 062 | 20.3 % | 40.9 % | 0.50 | 1.53 |

156 choices vetoed, 12 trades removed (+$7 392, 8 winners), one added
(+$1 546). Eight of the twelve were bearish fades against positive sentiment,
and those were the winners (COIN puts +$3 118, PLTR +$1 894, NFLX +$1 348).
The LLM's sentiment leans positive, so a sign gate mostly vetoes bearish
trades — and a mean-reversion fade is *by construction* a bet against the
move the news describes. Do not read the inverse (fade *against* the
sentiment) off this table: it would be chosen on the trades it is scored on.
The full-cache tercile table leans the same contrarian way (low tone +0.15 sd
at five sessions, high −0.07) and is equally in-sample; each weekly capture
adds weeks to test it on. Several decisions sat on near-zero sentiment
(TSLA −0.00 vetoed, IWM −0.00 kept), so a dead zone is the other open knob —
also not to be tuned on these weeks.

### The news study: what the headlines do not say, and what the calendar does

`scripts/news_study.py` (~10 s) writes `out/news_study.json`; the viz's **News
Study** page is the full report. Every row sits inside complete coverage, every
interval resamples weeks, and every searched family carries a placebo (news
shuffled across symbols within a day, or releases moved to random days, 200
times): a cell is a finding only if its |t| beats the shuffles' 95th-percentile
best. On the 2026-10-04 snapshot:

- **Kinds of news** (392 events, 9 weeks): no split by type, materiality, vol
  flag, horizon, read-through, arrival, attention or "priced in" survives
  (bars |t| 4.9 and 8.1; a cluster t on 9 weeks has heavy tails). Pooled, news
  describes its window (+0.12 sd), the next close continues it by +0.06
  [+0.02, +0.14], the fifth close is calm (0.76×), and ATM IV does not reprice.
- **Scheduled macro releases**: payroll sessions run 4.1× an ETF's non-release
  variance and 1.9× [1.33, 2.45] a single name's, beyond 98–100 % of
  random-day draws; CPI about 1×; FOMC afternoons 15×. ETF ATM IV falls 4.1 %
  over an 08:30 release session, below every random-day draw: a small,
  scheduled event premium, released at the print.
- **The sentiment indicator, ~75 variants** (half-life 2 h to a week, boxcar,
  trading clock, weights, floor, repeats, read-through, signal, optimism
  balancing, transform): raw tone forecasts nothing, because it is almost always
  positive. Balanced against the symbol's own history or its event type's
  running mean, short-memory tone leans to next-day continuation (+0.11 sd at
  12 h by event type, +0.20 at 6 h by symbol z, 6 of 7 weeks), and the day's own
  move does not explain it; best |t| 4.6 against a search bar of 7.0. Both are
  pre-registered (`LEADS`) and judged only on data after 2026-10-02.
- **The forecaster, offline and cross-fitted**: no news vol multiplier or drift
  improves the vol-timing model; the tone-change drift hurts (β +0.50 on one
  half, −0.74 on the other). Mean-reversion forecasts better after a big move
  with no news (+0.031) than one with news (+0.004), Chan's direction, within
  noise.
- **Day type**: vol-flagged morning news does not make trend days commoner.

So headline news stays out of the models, the macro calendar is the next vol
input, and the study re-runs on every weekly snapshot.

## ARK's daily trades: a contrarian readout, no forward edge yet

The capture syncs ARK's daily ETF trades (`ark_buy`/`ark_sell` events, from
2020, `after_close` on the trade day; `detail_json.etf_percent` is the fund
weight traded). `scripts/ark_study.py` is the event study: SPY-adjusted daily
moves in sd units, signed by the day's net ARK flow, entered at the next open
(the email lands after the close), weekly-block intervals.

The limit is our prices, not ARK's history: the snapshot's daily tape starts
2026-05-15, so the 2026-10-05 run has **100 ARK symbol-days over 20 weeks** on
9 cohort names (AMD 23 sells, PLTR 13s/4b; SPCX 13, COIN 10, META 9, NVDA 7,
TSLA 8b/2s buys; GS, UBER, XOM, AAPL, BA none in the window).

- **ARK trades against the recent move.** The 5 sessions before a trade ran
  −0.41 sd [−0.66, −0.16] in ARK's direction: it buys laggards and sells
  leaders — the same fade our mean-reversion model makes.
- **No reaction to the email**: the overnight gap is −0.03 [−0.14, +0.10].
- **Forward, nothing that survives the search.** Next session +0.11
  [−0.01, +0.24], fifth +0.04, twentieth +0.11. Demeaning each symbol's drift
  pushes d1 (+0.14) and d20 (+0.21) just clear of zero, but that is 2 of ~40
  cells, d20 windows overlap within 20 weeks, and the most independent rows —
  the leading edge of a run — show nothing (d20 −0.18).

What would settle it is daily bars back to 2020 for the cohort (a capture-side
backfill): ARK's history already holds 1 227 cohort trade days.

## The TA study: ten years of stock bars find vol, not direction

`uv run scripts/ta_study.py` (~8 min with 32 symbols; the level placebos are
most of it) writes `out/ta_study.json`; the viz's **TA Study** page is the
report. It reads the 5-minute TRADES backfill — every stock with ≥ 300 whole
sessions (32 on the 2026-10-06 snapshot, 2016-07 → 2026-10), with HYG, UUP,
USO, GDX and the VIX family as inputs only. A ticker's tape from before its
company traded is dropped (`LISTED`: HIMS was a flat $9.85 SPAC until
2021-01-21, and its zero-variance sessions once turned every one-session
move cell into NaN). The catalog (`scripts/ta_indicators.py`:
53 indicators, 124 signals, classic and modern, textbook parameters and
readings) was committed before the first result. Discovery is 2016 → 2022, the
holdout 2023 →, and every discovery finding is judged once on the holdout. A
statistic is the excess over the symbol's own period mean, in trailing-sd
units, entered at the next open; placebos shift each signal circularly by a
year or more per symbol, which keeps its base rate and breaks its timing.

Rerun on the 2026-10-06 snapshot (32 symbols; XLB, XLI, XLK, XLP, XLY, AMD,
MSTR and SPCX still waiting), against the first, 18-symbol run on 2026-10-05:

- **Direction: nothing.** 1 032 daily cells (signal × 1/5/10 sessions, raw and
  SPY-hedged × all / ETF / single): best |t| 2.6 against a bar of 4.0 (a
  shuffled search's median best is 3.1). On 15-minute bars the first run's 10
  discovery cells — all momentum into the close (above VWAP, above the upper
  band, RSI > 70, a rising MACD histogram), and all gone after 2022 — no
  longer pass even before 2023 with 32 symbols. No event
  is a better barrier entry than a random one, and no context filter (816,
  Connors' RSI(2) above the 200-day included), confirmation pair (55) or
  consensus count survives.
- **Move size: real, and it repeats out of period.** An earnings release in
  the next 5 sessions (+0.40 of log realized ÷ trailing vol before, +0.46
  after), the Bollinger and TTM squeezes, VIX9D/VIX, and — against Crabel —
  NR7/NR4 forecasting a *smaller* move. Intraday, relative volume, first-hour
  range and the gap forecast the rest of the session. 48 of 51 move leads
  survive (38 of 40 on the first run).
- **But HAR-RV already knows most of it.** Against log HAR-RV (daily, weekly,
  monthly realized variance off the 5-minute tape, pooled, fit on discovery),
  only the earnings calendar (vol × ~1.45 at five sessions, fit before 2023,
  CRPS +0.5 % after, both runs) and the overnight variance share still add
  CRPS; Bollinger bandwidth adds a hair (+0.04 to +0.09 %). The
  overnight share is partly built in: the target path starts at the next open.
- **The stock-only vol is HAR or a range estimator.** CRPS against 20-day
  close-to-close after the split: HAR +1.5 % next session, +1.2 % next five;
  Garman–Klass +1.2 % / +1.3 %, each scaled once on discovery (ranges miss the
  overnight gap). Unchanged by the larger universe.
- **SPY's variance premium, 2016 →:** realized ran below VIX-implied variance
  every year but 2018 and 2020. VIX9D's best scale as a five-session vol is
  0.78 before the split and 0.76 after; scaled, it edges HAR in both periods;
  unscaled it is no better than trailing vol. VIX prices the whole strike
  strip, so this is not the vol-timing model's 0.85 × ATM IV.

**Support and resistance levels (section H, catalog v2, added after the rest
was read): they mostly do not hold, and RSI confirmation is backwards.** Floor
pivots, the prior session's high/low/close, Fibonacci retracements of the
20-session range, the prior session's POC and 70 % value area, and the initial
balance, each scored at its first touch in a session (+/-0.25 daily sd
barriers over 60 minutes) against placebo levels set at the distance from the
open they had in another session (touch rates match, 60 sets):

- **No level passes its search bar on the 32-symbol run.** The pivot point
  leans toward holding (+0.020 → +0.018 barrier units) and the prior low,
  tested from above, toward breaking (−0.022 → −0.028), with holdout
  intervals clear of zero; both had passed on the 18-symbol run and shrank.
  The prior close, Fibonacci, the volume profile and the IB are noise.
- **An oversold touch of support (overbought of resistance) breaks far more
  at a real level than at a random one**: −0.04 pooled, −0.15 to −0.34 at the
  prior close, pivot point, POC and value area, in both periods and both runs.
  RSI alone is momentum at any price (−0.15). A rejection bar flatters every
  price (+0.39)
  and slightly less at real levels; volume, trend and confluence add nothing.
- **Confluence first looked like the strongest combination of all, and was a
  look-ahead**: it counted the first hour's high and low as confirming levels
  before 10:30. Counting only levels already known removed it entirely.

The Monday session (decide Friday, hold Monday's session) is positive in both
periods and on 29 of 32 symbols after 2022 (t 3.2) — not a discovery finding, so
unguarded, and recorded so nobody re-discovers it as new.

Mechanics: the 5-minute fold matches IB's daily bars to the cent on open, high
and low (1 393 days); the close differs (auction print against the last trade,
median 1.3 bp). A session must open at 09:30 and run a half day. Two runs give
identical JSON. `ta_indicators.py` uses Wilder's RSI and ATR, not the `ta`
crate's EMA-smoothed RSI or the engine's SMA ATR, so whatever is adopted is
ported to Rust and pinned to these values, as `pricing.py` pins the option
maths. Re-run when the backfill lands: the catalog is frozen (`VERSION`,
`POST_HOC`), so new symbols add evidence without adding search.

## Test fixtures generate their own options

`engine::testing::snapshot_with` builds a real on-disk snapshot whose ladder is
priced by Black-76 off a given path, so the parity fit recovers that path exactly
and every delta is analytic — a failure is then the engine's, not the fixture's.
`the_fixture_chain_reconstructs_its_own_underlying` checks that before anything
else concludes from it. Wrap the inserts in a transaction: without one the suite
takes 10.7s instead of 0.24s.

## Strategies are TOML over Rust traits

`strategies/mean_reversion.toml` is the worked example. **`mode` defaults to
`ranked`**; the retired staged pipeline (its configs gone, its engine dormant) must
be asked for with `mode = "staged"`. A section for the engine a config does not
run — `[signal]` in a ranked config, a `model` label in a staged one — is refused,
because a config that named `model` and `ranker` but forgot `mode` once ran the
staged pipeline under those labels. A ranked strategy composes five strict labels:
`model`, `ranker`, `portfolio`, `exit`, and `dataset`. They resolve TOML under the
matching plural directories; each field also accepts an inline table for one-off
runs. Genuinely new logic is still a trait impl (`PriceModel`, `PortfolioModel`,
or a staged `Signal`/`Structure`/`Selector`).
`StrategyConfig::validate` builds every stage up front, so a typo fails before any
I/O rather than after the first symbol has already run.

**The report's `config` is the *resolved* composition, not the labels**
(`StrategyConfig::resolved`, with the labels themselves under a separate
`labels` key). A report has to describe the run that happened: serializing the
config as written emits `model = "mean-reversion"` where `ib-option-viz` reads a
table — and, worse, emits the *default* `[candidates]`/`[ranking]` tables beside
a `ranker` label, so a credit-only run would report itself as allowing debits.
Both were true for one commit; `the_reported_config_describes_the_run_not_the_labels`
pins them shut.

A label **supersedes** the legacy inline fields it replaces — `model` supersedes
`[features]`, `ranker` supersedes `[candidates]`/`[ranking]`, `dataset` supersedes
`symbols`/`expiry`, `portfolio` supersedes `account`/`initial_cash`/`[sizing]` —
and writing both is **refused**, not silently resolved (`reject_shadowed_fields`,
which reads the raw TOML because a written field and a defaulted one are
indistinguishable once deserialized). A config must read the way it runs; that is
the same reason unknown keys are refused. The block enums deserialize by
dispatching on the value's shape rather than `#[serde(untagged)]`, so a typo
inside an inline block still names the misspelled key instead of the enum.

Labels resolve under `CARGO_MANIFEST_DIR`, so they follow the **source tree**, not
the working directory — a binary copied elsewhere still reads this repo's blocks.

`models/<label>/<dataset>.json` records the forecast gate (CRPS skill **and its
weekly-block interval**, PIT calibration, coverage, direction, and per-symbol
robustness) — one record per dataset, so a second dataset adds evidence rather
than overwriting the first. `rankers/<label>/<dataset>.json` records candidate
Spearman and decile monotonicity paired with a model label. Regenerate them with
`scripts/validate_model.py <label> --dataset <dataset>` (no strategy file: one
rendered run gives the pooled and per-symbol evals) and
`scripts/validate_ranker.py <strategy> --dataset <dataset>`. The gate
lives in `src/model/validation.rs` and the drivers repeat its thresholds, so
`every_checked_in_record_still_earns_its_verdict` recomputes every record's
verdict in `cargo test` — a record whose thresholds have drifted fails there
rather than sitting in the registry asserting a claim nobody applies.

**A ranker record grades the key that ranker actually orders by** — the
expected-P/L ratio for `expected_value`, the model probability of profit for
`prob_profit` (`ranker::ranking_key`). `RankedChoice::score` stays the EV ratio
because the portfolio model sizes against it; those are different questions, and
conflating them once made `rankers/prob_profit.json` a verbatim copy of `ev.json`
— a validation record about a scorer that run never used.

**Every field carries `#[serde(default)]`, and unknown keys are refused.** A
partial config that fails to deserialize is a live papercut in `ib-option-calc`
and not worth reproducing; a misspelled key silently taking its default would be
worse than an error.

`StrategyConfig::strategy()` returns a **fresh** strategy each call, and the
backtest command builds one per symbol. Signals carry indicator state — sharing
one would leak one symbol's price history into the next symbol's readings.

## The report says what it cannot support

**There is no Sharpe ratio, deliberately.** One expiry and ~44 sessions cannot
support one, and its only real function would be to make the result look more
settled than it is. `Report::caveats` is part of the output rather than a
footnote, and it flags: too few trades for the win rate to be more than
descriptive, how much P&L came from expiry settlement rather than a quoted exit,
rejections outnumbering fills, commissions dominating P&L, and symbols that never
traded.

A symbol with no two-sided bars is **skipped and named**, not run as a
zero-return symbol — averaging it in would dilute every real result.

The equity curve gets a final point at the settlement instant. Without it the
last point is the final option bar's *mark* — positions valued at what they could
have been sold for, a day before they actually settled — and anyone plotting
`equity.csv` would read a different ending than the summary states.

### What the main model actually does

`cargo run -- backtest --config strategies/mean_reversion.toml` over all 12
symbols on the shared $20k pool, as a sanity baseline (not a profit claim — 51
trades over one expiry proves nothing about P&L, and this one *loses*: −$5,510,
a 54.8 % drawdown). The forecast beats the null (CRPS skill +0.013, directional
hit-rate 56.9 %), and that shows up in the **structure mix**: 34 debit spreads,
12 naked longs, and only 5 credit — the directional forecast gives the ranker a
target to point long structures at. Contrast `driftless.toml`, which with no
directional view takes 4 trades, **all credit**. Exits are all on a real quote
(37 forecast-horizon, 5 forecast-target, 4 before-expiry, 3 before-earnings, 2
short-strike); settlement is zero. The forecast being right and the account still
losing is the whole reason the report leads with `model_eval` and treats P&L as a
guardrail — and why the sizing is a comparable axis (`equal_weight.toml` takes the
same 51 trades for −5.7 %). `scripts/compare_strategies.py` tabulates this against
every other variant.

## The ranked price-target engine is a second decision path, not a replacement

`mode = "ranked"` in a strategy TOML swaps the `signal -> structure -> selector`
pipeline for a different shape, while reusing the same bar loop, gates, timing,
portfolio, and settlement. `Strategy` is an enum (`Staged` | `Ranked`); the staged
path is byte-identical to before, which is why its golden tests never moved. Lives
under `src/model/`.

The ranked path, on each **option bar** (never the 5-min stock-only bars — the
feature engine is 30-min only, so a session VWAP cannot double-count overlapping
volume):

1. **`FeatureEngine::observe`** advances its indicators once, in order, and emits a
   `Features` vector — trend, VWAP distance, ATR, realized vol, ATM IV, put/call
   skew, DTE, time-of-day, days to/since earnings. Every history-dependent field is
   `Option`; a warming indicator or a one-sided ladder reports `None`, never a
   fabricated value.
2. **`PriceModel::forecast`** turns that into a `PriceDistribution` over the
   underlying at a scoring horizon. The v1 `LognormalDriftModel` is a lognormal:
   vol blended from realized vol and ATM IV and scaled to the horizon (floored by
   ATR), with a small drift from trend, VWAP reversion, and skew. It is close to a
   random walk unless its weights are turned up — the honest null. The trait is the
   seam: a richer model drops in without the ranker or engine changing.
3. **`enumerate_verticals`** walks every strike pair the ladder quotes, in both
   orientations, keeping those that price to a non-zero debit, are bounded, quote
   inside a relative-spread limit, and stay within the loss cap. Debit **and**
   credit verticals both fall out of one double loop — orientation is read from the
   sign of the entry debit, so credit spreads (which the fill model and portfolio
   already price) need no special case.
4. **`rank`** marks each candidate to model against the distribution and scores
   `E[P/L] / max_loss`, with a positive-expectancy floor and a minimum score. The
   best survivor is opened through the same next-bar timing and gates as staged.

   `min_prob_profit` is a further, *optional* floor on the odds themselves —
   refuse to open below this model probability of clearing the round-trip cost.
   It is not the same as `metric = "prob_profit"`, which takes the best odds on
   offer however poor they are, nor the same as `min_score`, which floors edge per
   unit of risk and will happily take a fat payoff that rarely pays. A
   short-premium thesis is "this expires worthless", and the gate is how that
   thesis is stated at entry rather than repaired at exit.

**Scoring is interim mark-to-model, not intrinsic-at-expiry.** `Spread::model_value`
(beside `payoff_at`) sums the legs' Black-76 values with time value at a
hypothesised forward, so a candidate is scored at a configurable pre-expiry horizon
rather than only at expiry. The assumptions, all in `ranking`/`model` config:
per-leg vol is held **sticky-strike** (each leg keeps its entry IV); the horizon is
capped at expiry, where `model_value` degrades to `payoff_at`; a sampled spot maps
to its forward by the current parity basis; and the risk denominator is the
**structural** expiry max loss from `payoff_bounds` (capital committed), not the
interim worst case.

**The funnel is the census here.** `RankingFunnel` counts the cascade —
enumerated -> priceable -> liquid -> within-loss -> positive-EV -> above-min-score
-> above-min-pop -> chosen — and the report surfaces it beside the trade log, the way
`signal_diagnostics` surfaces the staged gates. On the reference snapshot,
`mean_reversion.toml` prices 18 501 657 candidate verticals (8 732 187 before
interpolated strikes and the 0.20 leg-delta band) down to 49 taken. The
model decides how many bars hold a position and so are *not* enumerating, which
is why a more-selective model can enumerate more, not less. A ranked run whose
`chosen` is a rounding error against `enumerated` has been floored, not
backtested.

## The price model is evaluated, not assumed

The ranked path forecasts, so it is judged as a forecaster. On every option bar the
engine scores the model's distribution against the underlying's **realized** price a
horizon later — plus a random-walk null (driftless, vol = ATM IV) — and the report's
`model_eval` section aggregates it: CRPS skill vs the null (the headline), PIT
calibration and 50/80/90 % coverage, sharpness, and a directional hit-rate. The
per-bar forecasts (`forecasts.csv`) are two orders of magnitude denser than the
trades, so this is where tuning has statistical power; the trade P&L
(`profit_factor`, `avg_pnl_per_risk`, `predictions.csv`) is a guardrail.

The realized read looks *forward* in the already-loaded series. It is safe only
because it feeds evaluation and never a decision — see `src/model/eval.rs`. Do not
reuse that read anywhere a trade could see it.

**This is not academic — it drove the model we ship.** v1 shipped a *momentum*
drift (trend + skew) and forecast **worse** than a random walk (CRPS skill −0.39),
so the drift was zeroed to an honest null. Then a disciplined search on the dense
forecast eval found the real edge: at a **one-week horizon**, **fading** the
fast/slow trend gap (mean-reversion, `trend_weight = -0.5`) forecasts *better*
than the null — CRPS skill +0.013, 57 % directional hit-rate, robust across 9/12
symbols under leave-one-symbol-out. Momentum was exactly backwards: short horizons
are the reversion regime, momentum needs weeks-to-months. Two companion findings:
**ATM-IV-only vol** (`realized_vol_weight = 0`) calibrates best (pooled coverage
53/81/88), and the directional edge **decays to a coin-flip by two weeks** — so the
horizon is deliberately short. (A `validate()` guard once kept it ≤ the entry
blackout so every entry got the full horizon; chains are now scored to their
forced close instead — see *Expiry settlement*.)

**Correction, 2026-09-26: that edge was never established.** The gate then read
the point estimate. Forecasts inside one week share most of a seven-day horizon,
so they are nearly one bet, and `model_eval` now carries a **weekly-block
bootstrap interval** (`crps_skill_interval`, over `weeks` blocks). Read that way,
+0.013 is [−0.009, +0.031] over 10 weeks; it came mostly from AMD and MSTR
(−0.0017 on the ten symbols common to August and October); and October scores
−0.0095 [−0.026, +0.006]. The gate now requires the interval's **lower bound**
above zero, and **no model in the registry passes on either month** —
`mean-reversion-14d` on October is significantly *worse* than the null. The
ranker records, by contrast, hold (Spearman 0.38–0.45 August, 0.52–0.56
October): ordering candidates within a bar works even where direction does not.
BACKLOG.md records the resulting reframe — forecast move *size* against the
option market's price for it, not direction. Every directional finding in this
file predates the interval; re-check it against the gate before relying on it.

### The trend measure was never the lever; fading only against the prevailing trend is

`[features] trend_average` (`sma`, `ema`, `wma`, `vwma`) and a sweep of
fast 5/10/20 × slow 30/50/80 × weight −0.25/−0.5/−1.0 at 7 days (108 cells per
dataset, `scripts/sweep_model.py --jobs`): 84 cells beat the null on the
Aug-expiry chain, **0 of 108** on the Oct-expiry chain, and the Oct-expiry
"best" cells are just the smallest drift. The kind of average moves skill by
~0.005 against intervals ~0.04 wide. Session VWAP (`vwap_reversion`, −1 to +2)
adds nothing alone or on top of the trend, on either chain.

What the 10/30 gap cannot see is *which way the move runs against*. A spike up
in a rising market (AAPL drifting up through September) and a spike up in a
falling one (AMZN in a downtrend) read the same, and both get faded.
`prevailing_trend` is the slope of a 65-bar (5-session) SMA over 13 bars —
algebraically, the last session's average price against the one five sessions
earlier, divided by 5 — and `model.trend_with_prevailing_only` keeps the fade
only when it points the way the prevailing trend does. A spike with the trend,
or a warming prevailing trend (78 bars), gets no view. 65/13 was chosen before
any result and never tuned.

| | skill [interval] | dir % | sym+ | trades | return | max DD |
|---|---|---|---|---|---|---|
| Oct-expiry, `mean_reversion` | −0.0095 [−0.026, +0.006] | 52.2 | 5/15 | 52 | −28.8 % | |
| Oct-expiry, `mean_reversion_prevailing` | +0.0011 [−0.004, +0.008] | 59.0 | 10/15 | 18 | **+70.7 %** | 12.4 % |
| Aug-expiry, `mean_reversion` | +0.0133 [−0.009, +0.031] | 56.8 | 7/12 | 51 | −27.6 % | 54.8 % |
| Aug-expiry, `mean_reversion_prevailing` | +0.0007 [−0.008, +0.010] | 52.4 | 5/12 | 22 | −17.7 % | 51.4 % |

The Oct-expiry run is not one trade: 14 of 18 win, and it is +$7 499 without
its best three. But the rule was read off Oct-expiry charts, so that chain is
in-sample by construction; Aug-expiry is the test, and there P&L improves while
forecast skill goes to zero — the with-trend fades are where Aug-expiry's skill
lived (split across five prevailing definitions, Aug-expiry favours fading
*with* the trend every time and Oct-expiry *against* it every time). Neither
record passes the gate. A no-view bar is a driftless bar, so the ranker can
still open a credit spread there (3 on Aug-expiry, +$814; none on Oct-expiry).
The model reads only the sign of `prevailing_trend`; a dead zone around zero is
the obvious next thing to test, and each weekly capture adds a period to check
it on.

**A release's gap is the prevailing trend for the week after it**, because the
65-bar SMA spans five sessions. Bucketing the 2026-10-03 forecast log by
sessions since an opening gap of ≥1.5× the daily implied move: on the bars
where the model takes a view, skill is +0.003 with no big gap in five sessions,
**−0.04 to −0.05 in the five sessions after an earnings gap** (negative at every
threshold tried, 1σ–3σ), and mixed after non-earnings gaps. A fade there fights
post-earnings drift. `execution.post_earnings_blackout_sessions = 5` refuses
entries on the reaction session and the four after it (`census.
post_earnings_blackout`); `earnings_blackout_days` only ever looked forward.
Every strategy on a mean-reversion model sets it — all of them fade, and the
variants must keep differing from the base on one axis only. On
`mean_reversion_prevailing` it removes exactly four entries (TSLA after its
deliveries, AAPL, META, AMZN; −$1 878) and adds none: 40 → 36 trades, +$7 237 →
+$9 115; on `mean_reversion`, 91 → 81 trades, +$1 138 → +$1 989. One earnings season, 5–7 symbol-days — a risk rule on a known effect,
not a measured edge. Non-earnings gaps get no filter: nothing consistent there.

### The first vol-timing model: positive on both months, not yet significant

Fitting ATM IV × a multiplier on one month and scoring it on the other (Gaussian
CRPS in log space, weekly-block intervals, both directions):

- **Realized vol adds nothing beyond IV.** 1-, 5- and 22-day realized vol from
  the 5-minute tape (overnight gaps included), alone or blended, fit a weight of
  ~0 and score no better than a flat rescale. At a week out, IV already knows
  what the stock's recent vol knows.
- **Outside earnings, moves run below IV** — mean (move ÷ implied)² 0.69, 0.82
  and 0.74 in the three periods the months do not share, 0.28–0.67 on the ETFs.
  The variance premium, the one pattern with real sample size behind it.
- **A week containing earnings moves ~2× the monthly IV's implied move** — but on
  12 distinct events, 6 above implied and 6 below, and the months overlap
  (2026-07-20 → 08-20) so they largely saw the same late-July prints. A monthly
  IV spreads the jump over the month; the market's own event pricing is in the
  near-over-far IV, which the weekly multi-expiry capture will supply.

`models/vol-timing.toml` is the result: driftless, `vol_multiplier = 0.85`,
`earnings_vol_multiplier = 2.0` (round numbers between the two fits, so neither
record is strictly out of sample). It is the **first model positive on both
months** — +0.0084 [−0.011, +0.029] and +0.0065 [−0.008, +0.023] — with the best
calibration of any (PIT error 0.095/0.086, 50 % coverage 48–49 % against the
null's 53 %), and it does not pass: both intervals include zero.

Its null is `PriceModel::null_forecast` — driftless, vol = ATM IV, multipliers
ignored — so the eval compares the model with the market rather than with
itself. The gate's robustness test is now *per-symbol CRPS skill* (two thirds of
symbols positive) rather than per-symbol direction, and a model that forecasts no
direction records `directional_hit_rate: null` and owes that test nothing.

**Do not read its backtest.** Traded through the directional-era `base` exits, it
buys naked longs into earnings weeks and then `close_before_earnings` shuts all 8
of them before the release they were bought for (August: 12 trades, +25.7 %;
October: 1 trade). Exits and structures have to follow the model (BACKLOG Phases
2–3) before its trades mean anything.

### HAR-RV and tape-sized earnings in the vol: built, measured, not adopted

The TA study's three vol takeaways, taken to the forecast gate on the
2026-10-06 snapshot. `ChainSeries` now computes each whole session's realized
variance off the 5-minute tape it already loads (squared 5-minute returns plus
the overnight gap, as `ta_study.py` does), giving HAR-RV's day/week/month
inputs from completed sessions before the decision date — read off the stock
tape before the option tape starts, so HAR exists from a chain's first bar — and
each symbol's mean results-session variance over the month before, across
releases already past. Two model switches, both off by default (the original
path is untouched; goldens unchanged):

- `har_weight` blends HAR's vol with ATM IV in variance; `vol_multiplier`
  then scales the IV alone. Coefficients are the study's pooled 2016–22 fit
  (`har_coefficients`, `har_resid_var`, `har_scale` 0.92) — nothing fit on the
  option window; the weight 0.5 is a round prior.
- `earnings_from_tape` replaces the flat `earnings_vol_multiplier` with each
  symbol's event variance `e`: a horizon of `n` sessions holds `(n - 1 + e)/n`
  normal sessions' variance. HAR takes all of it; the IV only the part its
  expiry (`N` sessions) has not already spread, `((n-1+e)/n) / ((N-1+e)/N)`.
  On the tape `e` runs from ~4 (banks, energy, TSLA with its deliveries) to 33
  (META); the IV bump 1.17–1.68, every one below the flat ×2.0.

| model | Aug-expiry | Oct-expiry |
|---|---|---|
| `vol-timing` (re-run on 10-06) | +0.0072 [−0.012, +0.028] | +0.0064 [−0.006, +0.022] |
| `vol-timing-har` | +0.0023 [−0.016, +0.021] | +0.0078 [−0.002, +0.017] |
| `vol-timing-tape` | −0.0033 [−0.019, +0.012] | +0.0065 [−0.005, +0.022] |
| `vol-timing-har-tape` | −0.0066 [−0.021, +0.007] | +0.0080 [−0.001, +0.017] |

None passes; nothing separates. Split by a release inside the horizon:
**outside earnings weeks HAR is a wash** (Aug +0.0005 → −0.0035, Oct +0.0042 →
+0.0056) — over the option window it adds nothing ATM IV does not already
know, now with the strongest stock-only estimator rather than 20-day
close-to-close. **Inside earnings weeks the tape bump trades August's point for
a much tighter interval** (−0.017 [−0.069, +0.028] against +0.032 [−0.093,
+0.108]; October +0.035 [−0.031, +0.046] against +0.033 [−0.115, +0.061]): the
late-July prints moved ~2× the monthly IV's implied move, more than their own
history said. About a dozen events a month cannot choose between them, so the
strategy stays on `vol-timing`; the labels are frozen as the pre-registered
comparison for the Q3 earnings season the October and November chains are
now capturing.

**VIX9D against SPY's ATM IV**: 1.08× (median, 64 days 06-23 → 09-24; the
feature chain's ATM IV, usually a monthly). So the study's 0.77 × VIX9D is
0.83 × ATM IV — a decade of SPY lands beside the 0.85 the vol-timing model fit
on two months. `vol_multiplier` stays.

### Direction-neutral structures, the crush, and trades held through earnings

`rankers/vol.toml` enumerates only structures that bet on the **size** of the move
(`CandidateConfig::allow_straddle/strangle/iron_condor/iron_butterfly`): long
straddles and strangles, and iron condors and butterflies with wings
`1..=wing_steps` strikes out. All defined-risk — `Spread::payoff_floor` refuses
only a runaway *loss*, so a long straddle's open upside is allowed and a naked
short never is. `Spread::is_direction_neutral` (both rights present) keeps them
out of every direction-reading rule: no forecast-target cap in scoring, no
target/stop bounds at entry, no momentum exit.

**Crush.** With `crush_to_median`, when a release falls inside the horizon the
ranker revalues each leg at the symbol's trailing-median IV instead of its entry
IV (`PriceModel::horizon_vol_scale`, never above 1). Black-76 here reads vol and
time only through σ√τ, so the engine applies it as a k² factor on the chain
forecast's remaining time — no signature anywhere changes. Sticky strike across a
release overvalues every long-vol position; on August the crush alone moves the
vol-timing run from −6.4 % to −4.7 %.

**Held through earnings** is a flag on every closed trade (open on or before a
release's last clean session, closed after) and a line in the totals
(`earnings_trades`, `earnings_pnl`) and caveats — a few binary events, reported
apart so they cannot hide in, or swamp, the rest. `strategies/vol_timing.toml`
runs with no earnings guards (`exits/vol.toml`, no blackout).

First runs, one lot each (guardrail, not evidence):

| | trades | return | strangles through a release | iron condors/flies |
|---|---|---|---|---|
| August | 16 | −4.7 % | 9, −$1 491 | 7, +$557 (6 of 7 up) |
| October | 7 | −4.6 % | 6, −$321 | 1, −$381 |

The structures follow the model exactly — long vol 5–7 days before a release,
short vol on ETFs and quiet megacaps — and the ranker orders them (scorer
Spearman 0.41 / 0.46). What fails is the flat `earnings_vol_multiplier = 2.0`:
every earnings strangle scored strongly positive (AMD +$1 355 expected, −$1 525
realized) and the 15 of them lost $1 812. That multiplier is the number the
multi-expiry term structure is meant to replace.

### Vol-aware exits: close the morning after the release

`exit.close_after_earnings` is the vol model's answer to `close_before_earnings`:
hold through a release, then close on the first bar after it is priced
(`ExitReason::AfterEarnings`, `ChainSeries::last_release_before`). Variants of the
vol-timing exits, both months, one lot:

| exits | Aug return | long vol | short vol | Oct return | long vol |
|---|---|---|---|---|---|
| base (target 50 %, horizon cap) | −4.7 % | 9, −$1 491 | 7, +$556 | −4.6 % | 6, −$321 |
| **+ close after earnings** | **−2.8 %** | **10, −$1 107** | 7, +$556 | **−4.0 %** | **9, −$184** |
| + target 25 % | −3.9 % | 10, −$1 107 | 9, +$327 | −4.0 % | 9, −$184 |
| + no target | −2.5 % | 10, −$1 107 | 7, +$603 | −4.0 % | 9, −$184 |
| + no horizon cap | −4.0 % | 9, −$1 115 | 6, +$309 | −10.7 % | 6, −$123 |

Closing after the release helps long vol in both months — out before the rest of
the crush and the post-event drift — so `exits/vol.toml` does it. It does not make
the earnings trade pay: the flat ×2.0 multiplier still overprices the event.
Short-vol profit-taking at 50 % and none are equivalent on 7 trades; 25 % gives
up too much. Dropping the horizon cap leaves positions open, marked, at the end
of the data. None of these is a sample; the direction of the long-vol change is
the only part seen twice.

**"Earnings" means "results release".** EDGAR Item 2.02 carries TSLA's quarterly
deliveries too (52 near-pairs since 2013) and occasional pre-announcements
elsewhere (AAPL's 2019-01-02 revenue warning). They move the stock, so a vol model
should see them; they stay. Two real gaps: XOM has **one** event in the whole
table (its history never synced), and EDGAR knows a release only once filed — a
backtest reads dates in hindsight that live trading will need a forward earnings
calendar for.

### The index put premium: real, thin, and only a skewed condor keeps it

`scripts/option_premium.py` measures the premium straight off the quotes, with no
model: every option entered at 15:00 UTC and held a fixed number of days. `map`
delta-hedges each option at its entry delta, so the market's direction is taken
out; `structures` prices defined-risk ETF structures leg by leg, crossing every
leg both ways plus commission. Re-run it on each capture — each adds a period.

**Correction to this file: the indices did not rally.** Jun 17 → Sep 25 SPY rose
4.1 %, QQQ 3.0 % (after a −3.7 % dip to Jul 20), and IWM *fell* 2.7 % (−5.3 % from
Aug 21 to Sep 18). Findings above that blame a "rising sample" — the RSI regime,
the losing credit spreads, the PIT at 0.53–0.56 — were reading forecasts pooled
across all symbols, where PLTR and COIN ran. The model underestimated high-beta
upside; it was not fading an index rally.

- **ETF puts are overpriced, delta-hedged, in every period** — the seller's
  7-day return on premium at mid is about +7 % at 1–3 % out of the money, +11 % at
  3–6 %, +14 % at 6–10 %, the same in the rising, the churning and IWM's falling
  month (IWM's puts were richest in the month it fell: +15 to +27 %). ETF calls
  near the money are about fair; further out they are cheap. Single-stock options
  outside earnings are about fair at mid, and their 3–8 point spread leaves
  nothing. In earnings weeks single-stock options were *cheap* (ATM −18 to −21 %
  to the seller) — on one cluster of late-July prints.
- **Hedging needs margin** (short stock against a short put), so it is out for a
  $20k no-margin account. Defined-risk spreads are collateralised by max loss,
  not borrowed — the backtester's `capital_at_risk` already models that.
- **Put spreads keep none of it.** They are long the market, and their protective
  wing is the most overpriced option on the board. At 21 days, −2.8 to −0.9 % per
  trade, fills at mid or crossed.
- **The crossing cost is the other half.** At mid the put spreads collect about
  +1.3 % of max loss a week hedged; crossing two legs in and out costs the same
  at 7 days, a third of it at 21–28 (cost per round trip, premium per week).
- **The skewed condor keeps it**: short puts 4.5–6.5 % out (the rich skew), short
  calls 1–3 % out (priced about fairly, so they offset the direction cheaply). At
  21 days, +11.9 % per trade on max loss [+1.0, +22.4], at 28 days +16 to +21 %.
  But QQQ carries it (IWM 0 to +6 %), September entries lost ~20 %, and it cannot
  be placed on SPY at all — the capture band stops at about −5 %, so there is no
  6.5 % put. `fixed_tolerance` refuses a leg more than 1 % of spot from its
  target: before it, "nearest to −6.5 %" was the last strike quoted and SPY's
  wing a $1 spread, which the first version of this analysis measured without
  noticing.

`strategies/skewed_condor.toml` trades it through the engine: `fixed_legs` in
`rankers/skewed_condor.toml` (one candidate per bar, floors off, so it is taken
whenever the gates allow), `max_hold_days = 21` (`exits/hold_21.toml`,
`ExitReason::HoldPeriod`), a ladder of up to three per ETF spaced by
`min_days_between_entries = 7`, and `entry_window = ["10:00", "15:00"]` New York
time, since a session's first and last bars quote about twice the spread. On the
ETF datasets, one lot each: August chain 10 trades, +3.5 %, 9 winners, drawdown
10.6 %; October chain 10 closed (+$251) and four still open when the data ends,
marked −$637 after QQQ jumped 3 % through their short calls — a rally through
the near-money call spread is this structure's risk. A lead, not an edge: two
ETFs, ~20 trades, and fill quality — the swing factor — only measurable with
real orders.

The null-beating gate is the floor: a config that cannot beat a random walk
out-of-sample is not a better forecaster, whatever P&L it posted. And the converse
holds too — mean-reversion is adopted on its **forecast** merit, not its backtest
P&L, which on ~30 trades over one expiry is too noisy to trust. The trade log is a
guardrail; `forecasts.csv` (two orders of magnitude denser) is where tuning has
power.

### The strategy variants — one framework, single-axis comparisons

The framework has five labeled axes — **price model**, **ranker**, **portfolio**,
**exit**, and **dataset**. `strategies/` holds one config per interesting point, each
differing from the `mean_reversion` base on exactly one axis, so a comparison
isolates that axis:

| Config | Axis | What it is |
|---|---|---|
| `mean_reversion.toml` | — | The main model (IV vol, `trend_weight -0.5`, 1-week horizon, EV scorer). Debit + naked mix. |
| `driftless.toml` | model | The honest random-walk null (`trend_weight 0`). Credit-heavy theta harvest, ~0 skill. |
| `prob_profit.toml` | scorer | `metric = "prob_profit"` — rewards hit-rate over magnitude. |
| `exit_pop_trailing.toml` / `exit_momentum.toml` | exit | Structure-scoped give-ups: live-POP on debits, trailing vs momentum on nakeds. |
| `equal_weight.toml` | portfolio | Fixed one-lot allocation instead of score-weighted sizing. |
| `mean_reversion_prevailing.toml` | model | Fade a spike only against the 5-session prevailing trend. Oct-expiry +70.7 %, Aug-expiry −17.7 %; in the frontier. |
| `mean_reversion_prevailing_sentiment.toml` | execution | The prevailing fade, vetoed when news sentiment disagrees with its direction. 49.6 % → 20.3 %: it vetoes the winning fades. |
| `mean_reversion_no_horizon.toml` | exit | The base without the forecast-horizon cap. Halves the loss; nakeds gain, debit spreads lose. |
| `naked_rsi_trailing.toml` | model+ranker+exit | Nakeds at RSI extremes on a trailing stop. Loses 53.9 %; the overbought half is 9/10 of it. |
| `naked_rsi_recovery.toml` / `_tight.toml` | execution | Enter after RSI turns up through 30, within N bars. First positive arms — and one trade is ~all of it. |
| `naked_rsi_oversold.toml` / `_bull.toml` | execution, ranker | The oversold half alone: −5.1 %. Forcing calls changes nothing — it already picked calls. |
| `mean_reversion_rsi.toml` | execution | The base refusing entries above RSI 70. −27.6 % → −5.0 %; regime-conditional. |
| `credit_pop.toml` | ranker | Credit spreads only, ordered by probability of profit. Its scorer discriminates best of the set (Spearman 0.45) and it still loses — see below. |
| `credit_pop_stop.toml` / `_conviction.toml` / `_flat.toml` | exit, portfolio | The adverse-forecast stop, then the same 35 trades sized three ways — the arms that showed the score does not size. |

The first six run as one shared $20k account (the default), so their P&L is
comparable; the `credit_pop_stop_*` trio deliberately varies the sizing.

**The exploratory credit configs have been retired, and their findings have not.**
The POP-floor sweep, the 70 %-odds entry gate, the reward/risk band and the whole
horizon 2×2 each answered their question — the numbers are recorded in the
sections above and the configs are in git history. What stays is the *vocabulary*
they were written in: every block they composed is still under `models/`,
`rankers/`, `exits/` and `portfolios/`, including `credit_balanced`,
`credit_pop_high`, the three `pop_floor*` bundles and the 14-day models with their
validation records. Re-running any of them is a four-line strategy file.

**14-day credit spreads are on hold.** Not because the idea failed — the vol
scaling held and a model cleared the gate — but because the pass rested on a
single held-out symbol from a search over one snapshot, which is a question about
data rather than about modelling. The second expiry answered it: re-run under
the interval gate, `mean-reversion-14d-slow` is −0.0065 [−0.016, +0.003] on
October and `mean-reversion-14d` −0.0048 [−0.009, −0.0003] — worse than the
null. Neither survives out of period, and neither passes on August either once
the interval is read.

`scripts/compare_strategies.py` runs them all and tabulates the forecast headline
(CRPS skill, directional hit-rate) beside the trade guardrail (return, drawdown,
return/drawdown, profit factor, structure mix). The staged pipeline (`signal → structure → selector`)
is retired: its strategy configs and file-golden tests are gone, though the engine
code stays dormant behind `Strategy::Staged` and keeps its inline unit tests. A
future **RSI-oversold** model would reuse `indicators::Rsi` (independent of the
staged pipeline) as a new drift term, validated on the same null-beating gate.

## The frontier on three years of tape: measured 2026-10-21, not yet trustworthy

The frontier ran on the research DB built 2026-10-21 (IB plus UW's tape, `history.toml`: 37 monthlies, 2023-11-17 → 2026-11-20). It took 96 minutes and peaked at ~5 GB once a symbol's stock tape was shared across its chains (`UnderlyingCache`). These are the results **before the stale-quote rule** (below, with the results after it). Every strategy loses:

| Run | Trades | Avg P&L | Win |
|---|---|---|---|
| `vol_timing_history` | 974 | −65 | 27% |
| `skewed_condor_etf-history` | 444 | −46 | 44% |
| `mean_reversion_history` | 2 221 | −293 | 32% |
| `mean_reversion_prevailing_history` | 1 896 | −391 | 30% |

**Don't read those as the strategies' verdict yet. The tape biases them.** The seam check (ib-option-calc's `research.py build --tape-only`) took all.toml's three monthlies, the only ones IB's capture quotes (08-21, 10-16, 11-20), and read them three ways. Inside the IB run's own entry window (Jun 24 → Oct 12, 2026), the average trade was:

| Strategy | IB capture only | Full (IB + tape) | Tape only |
|---|---|---|---|
| `vol_timing` | +103 (21) | +17 (41) | −31 (57) |
| `skewed_condor` | +43 (31) | +25 (40) | +16 (38) |
| `mean_reversion` | −41 (101) | −174 (134) | −247 (169) |
| `mean_reversion_prevailing` | +243 (39) | −305 (76) | −309 (132) |

- **The more of a run's quotes come from the tape, the worse it does, and the more it trades.** The condor, which picks a fixed structure, barely moves. The strategies that rank candidates by price are hit hardest. That is selection on noisy prices: a tape quote is the NBBO at the bar's last print, up to 15 minutes stale, so the candidates that look best are disproportionately stale quotes. The spread correction fixes the average cost, not this.
- **all.toml on a research DB is a different backtest.** It lists three monthlies, so with the tape's history behind them the front month is months out (median 79–135 DTE at entry outside the IB window, which capture data never offered). On a research DB, compare with the `history` rows.
- **The shared account trades through ruin.** Sizing bounds capital *at risk* by `initial_cash`, not equity, so mean reversion's equity ends near −$634k and it is still opening trades. Per-trade averages and win rates are the meaningful numbers there; cumulative dollars aren't.

### Stale tape quotes: entries decide on quotes under a minute old

A tape quote is the NBBO at its bar's last print, and its error against IB's quote for the same contract-bar grows with how long before the bar's end that print came: a median 0.25 IB spreads within the last minute, 0.65 at 10–15 minutes. The median tape quote is 4 minutes old (ib-option-calc's `docs/research-db.md`). The research DB carries each tape quote's `age` (UW quotes v3), and `execution.max_entry_quote_age_secs` decides entries on a view of the bar without the older ones (`ChainSeries::snapshot_fresh_at`). A dropped strike is interpolated from the bar's fresh quotes like any strike that didn't print. Exits, marks and fills still read every quote, and IB's quotes are never stale, so a run on a capture snapshot is unchanged.

Dropping stale quotes from the build instead (`research.py --max-tape-age`) starves the smile everywhere, exits included, and hurt the condor most. In the seam check, inside IB's window (average trade, trades):

| Strategy | IB only | Full, no rule | Full, entries ≤ 180 s | Full, entries ≤ 60 s |
|---|---|---|---|---|
| `vol_timing` | +103 (21) | +17 (41) | +62 (29) | **+70 (27)** |
| `skewed_condor` | +43 (31) | **+25 (40)** | +8 (41) | −4 (39) |
| `mean_reversion` | −41 (101) | −173 (134) | −29 (116) | **−32 (108)** |
| `mean_reversion_prevailing` | +243 (39) | −305 (76) | −27 (50) | **+95 (45)** |

So the three ranked strategies set it to 60 s (`strategies/*.toml`), and the condor, which picks a fixed structure and needs real wing quotes, doesn't. The rule closes most of the gap, not all of it: `mean_reversion_prevailing` still makes 95 a trade where IB alone makes 243.

**The history with the rule** (2026-10-21, same research DB, ages on):

| Run | Trades | Avg P&L | Win | Tape era | IB era | Before the rule |
|---|---|---|---|---|---|---|
| `vol_timing_history` | 373 | −28 | 32% | −38 (342) | +86 (31) | 974, −65 |
| `skewed_condor_etf-history` | 444 | −46 | 45% | −56 (402) | +50 (42) | unchanged |
| `mean_reversion_history` | 1 371 | −149 | 39% | −152 (1 225) | −120 (146) | 2 221, −293 |
| `mean_reversion_prevailing_history` | 772 | −237 | 39% | −232 (689) | −275 (83) | 1 896, −391 |

Every strategy still loses before 2026-06, in every year (2023 is the worst for all four). The rule roughly halves the ranked strategies' losses and their trade counts, which is the stale-quote selection going away. What's left is either a real absence of edge or tape bias the rule doesn't reach. One sign of the second: `mean_reversion_prevailing` makes +82 (47) in the IB era on all.toml's three IB-captured chains, and loses −275 (83) in the IB era of the history, whose extra chains (06-18, 07-17, 09-18) only the tape quotes. Until a tape-era finding is checked against IB-era chains IB captured, treat it as unconfirmed; the condor's result is the closest to source-independent.

## The account model: one shared pool is the default

A backtest runs **every symbol against one `initial_cash`** — the default
`portfolio = "shared_20k"` label uses a shared account with score-weighted sizing
(`10%/25%/5-lot`). `equal_weight_1` swaps only the allocation rule. An inline
portfolio with `account = "per_symbol"` opts back into an account per symbol
(`run_symbol` each, `report::build` summing them).

The shared account is the default because per-symbol sizing was quietly running
*N* independent accounts: a `$20k` run was `$20k × 9 symbols ≈ $180k`, and the
headline was their sum. Only a shared pool answers "what does one limited-capital
account do", so it is the honest basis for comparing strategies. `run_shared`
iterates a **single global timeline merged across all symbols** —
sequential-per-symbol cannot model two names competing for the same dollars *at
the same time* (it either double-counts or serializes them). The defaults live in
`portfolios/shared_20k.toml`: shared account, $20k cash, one position per symbol,
and the `ScoreWeightedSizer` policy.

Per timestamp, in order: execute due closes (frees capital) → fund the due opens
**best-score-first** across symbols, each sized at fill against the capital still
free → mark every symbol's positions and record one account equity point →
decide (exits + entry) and queue for each symbol's next bar. Consequences that
are deliberate, not bugs:

- **Allocation moves to fill time** on this path (`Intent::Open` carries an
  `AllocationHint`; `execute` calls the selected `PortfolioModel`), because lots must be counted
  against the capital actually free when the order fills, not one bar earlier.
  The per-symbol path still sizes at decision time — its behaviour is unchanged.
- **Low-value trades get squeezed out when capital is scarce** — best-first
  funding plus the portfolio policy's remaining-budget cap means a full account funds only
  the top scores; the rest count `census.capital_limited`.
- **One position per symbol** (`max_positions_per_symbol`, defaulted to 1 in
  shared mode) so capital diversifies across names rather than piling into one.
- **`capital_at_risk` is what `initial_cash` bounds**, summed across all open
  positions (a credit spread's margin, a debit's premium, a naked long's
  premium). `the_shared_account_runs_all_symbols_against_one_pool` pins that peak
  risk never exceeds the pool and ≤1 position is open per symbol.

`SharedRun` carries the one portfolio + one equity curve + per-symbol
`SymbolDiag`s; `report::build_shared` turns each symbol into a *view* over its
slice of the shared trades, so the per-symbol columns and model/scorer eval
aggregate exactly as before while the totals come from the single pool. Existing
report fields remain stable; scorer decile monotonicity and return/drawdown are
additive fields, so the viewer can continue rendering the contract as-is.

The `shared_20k` score-weighted policy blends two caps: a **risk cap** bounds dollars per
name (`risk_per_position`, scaled by score up to `max_risk_per_position`) and a
**lot cap** (`max_lots`) bounds fill size — the risk cap binds on expensive
spreads, the lot cap on cheap ones, so neither a single name nor a single fill
runs away. There is no market-impact model, so the lot cap is the stand-in that
keeps a large fill from clearing at the one-lot quote. Every backtest is this
shared $20k account at 10%/25%/5-lot by default. The caps *bound* the account,
they do not smooth it: the directional model carries naked longs, so its curve is
more volatile than a pure credit harvester's.

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
- Tests are inline `#[cfg(test)] mod tests` blocks. The one exception is
  `tests/golden.rs`, which runs against the real snapshot and **skips cleanly
  when none is present** — the file is 213 MB and lives outside the repo.
- **The crate isn't rustfmt-clean.** `cargo fmt` reformats about twenty files nobody
  touched. Format only the files you change (`rustfmt --edition 2024 <file>`).
- `--db` takes a capture snapshot or a **research DB** (ib-option-calc's
  `docs/research-db.md`: IB's data with UW's tape merged in, back to 2023-11).
  `Snapshot::format` says which; the loaders have a research form of each query
  returning the same columns, and `a_research_db_loads_exactly_what_its_snapshot_does`
  pins that. Every `backtest.json` records which file it read (`snapshot`), which is
  what `scripts/publish-results.sh` cuts each run's bundle from.

## When a golden test fails

`tests/golden.rs` pins values confirmed by hand: SPY's parity forward at a known
bar, the quality account, and the full runs of the ranked variants
(`driftless`, `mean_reversion`, `exit_pop_trailing`) down to the cent — funnel counts,
P&L, structure mix, and the forecast-eval sign (driftless ~0 skill,
mean-reversion > 0). They are deterministic.

They read the **reference file by name** (`ib-option-calc-snapshot-2026-08-28`),
not the newest snapshot: every later capture is a superset that moves some count.

So decide which happened before touching one: a regression, or a deliberate
change to the model. Only the second is a reason to edit the expected value, and
the commit should say which. If `the_catalog_reports_one_expiry_across_twelve_symbols`
fails, a *different snapshot* is being tested and every other pinned value should
be re-confirmed by hand rather than adjusted to fit.
