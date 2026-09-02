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

**`~/ib-option-calc` is read-only. `~/ib-option-viz` is writable.** The capture
tool owns data this repo can never regenerate, so nothing here touches it. The
viz is the read side for the reports this repo writes, and the two halves of a
change — a new field and the panel that renders it — belong together.

When changing the viz: it has its own `CLAUDE.md`, its own conventions and its own
golden tests (`uv run pytest`, 60 of them, skipping cleanly without a snapshot).
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
- **A position the current bar cannot price is held at entry cost**, not marked
  to zero. Wings stop printing; writing them off would invent a total loss the
  market never showed.

`Spread::payoff_bounds` derives max profit and max loss generically, by sampling
the strikes and both tails, rather than special-casing verticals — so a butterfly
or condor added later gets correct bounds for free. It returns `None` for a
structure with a runaway tail (a naked call, either side), because a profit
target expressed as a percentage of max profit is meaningless there. Naked
**puts** are bounded: the underlying cannot go below zero.

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


## Test fixtures generate their own options

`engine::testing::snapshot_with` builds a real on-disk snapshot whose ladder is
priced by Black-76 off a given path, so the parity fit recovers that path exactly
and every delta is analytic — a failure is then the engine's, not the fixture's.
`the_fixture_chain_reconstructs_its_own_underlying` checks that before anything
else concludes from it. Wrap the inserts in a transaction: without one the suite
takes 10.7s instead of 0.24s.

## Strategies are TOML over Rust traits

`strategies/mean_reversion.toml` is the worked example; the shipped configs are all
`mode = "ranked"` (the staged three-stage pipeline is retired — its configs are
gone, its engine dormant). A ranked strategy composes five strict labels:
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

`models/<label>.json` records the forecast gate (CRPS skill, PIT calibration,
coverage, direction, and per-symbol robustness). `rankers/<label>.json` records
candidate Spearman and decile monotonicity paired with a model label. Regenerate
them with `scripts/validate_model.py` and `scripts/validate_ranker.py`. The gate
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
`mean_reversion.toml` prices 8 732 187 candidate verticals down to 51 taken. The
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
horizon is deliberately short and, via the guard in `validate()`, must stay ≤ the
entry blackout so a scored entry always gets the full horizon.

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
| `mean_reversion_no_horizon.toml` | exit | The base without the forecast-horizon cap. Halves the loss; nakeds gain, debit spreads lose. |
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
data rather than about modelling. `models/mean-reversion-14d-slow.json` and
`mean-reversion-14d.json` are kept exactly so a second expiry can re-run
`scripts/validate_model.py` against the same labels and see whether the result
survives out of period.

`scripts/compare_strategies.py` runs them all and tabulates the forecast headline
(CRPS skill, directional hit-rate) beside the trade guardrail (return, drawdown,
return/drawdown, profit factor, structure mix). The staged pipeline (`signal → structure → selector`)
is retired: its strategy configs and file-golden tests are gone, though the engine
code stays dormant behind `Strategy::Staged` and keeps its inline unit tests. A
future **RSI-oversold** model would reuse `indicators::Rsi` (independent of the
staged pipeline) as a new drift term, validated on the same null-beating gate.

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

## When a golden test fails

`tests/golden.rs` pins values confirmed by hand: SPY's parity forward at a known
bar, the quality account, and the full runs of the ranked variants
(`driftless`, `mean_reversion`, `exit_pop_trailing`) down to the cent — funnel counts,
P&L, structure mix, and the forecast-eval sign (driftless ~0 skill,
mean-reversion > 0). They are deterministic.

So decide which happened before touching one: a regression, or a deliberate
change to the model. Only the second is a reason to edit the expected value, and
the commit should say which. If `the_catalog_reports_one_expiry_across_twelve_symbols`
fails, a *different snapshot* is being tested and every other pinned value should
be re-confirmed by hand rather than adjusted to fit.
