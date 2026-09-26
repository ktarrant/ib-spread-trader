# ib-spread-trader

Backtests modular option-spread strategies against captured Interactive Brokers
history. Built to validate algorithms before they trade live through the IB API;
**it places no orders, live or paper, and opens its data read-only.**

A ranked strategy is five composable, reusable labels:

```
PriceModel -> Ranker -> PortfolioModel -> ExitModel    (over a DataSet)
```

Each block is a strict TOML config, and model/ranker labels carry auditable JSON
validation records. Parameters change without recompiling; genuinely new model or
allocation logic remains a small Rust trait implementation.

## Quick start

Needs a capture snapshot from [`ib-option-calc`](../ib-option-calc) in
`~/Documents/Options Database` (or set `$IB_SNAPSHOT`).

```sh
cargo test
cargo run -- catalog                                       # what is backtestable
cargo run -- chain --symbol SPY --expiry 2026-08-21 \
                   --at 2026-08-19T15:00:00+00:00          # one resolved ladder
cargo run -- backtest --config strategies/mean_reversion.toml --out out/
python3 scripts/compare_strategies.py
python3 scripts/validate_model.py strategies/mean_reversion.toml
```

Every subcommand prints JSON to stdout and nothing else, so it pipes into `jq`.
`--out` additionally writes `trades.csv` and `equity.csv`.

## A strategy composition

```toml
name      = "mean-reversion"
mode      = "ranked"
model     = "mean-reversion"
ranker    = "ev"
portfolio = "shared_20k"
exit      = "base"
dataset   = "2026-08-21"   # a label whose file sets `expiry`, or `expiries = [...]`

[execution]
timing = "next_bar"
earnings_blackout_days = 3

[costs]
commission_per_contract = 0.65
```

Labels resolve under `models/`, `rankers/`, `portfolios/`, `exits/`, and
`datasets/`. Any block can instead be an inline table for an experiment. The
shipped strategy variants change one label at a time, including a driftless model
null, probability-of-profit ranker, two exit bundles, and equal-weight portfolio.

## What it refuses to do

Most of the design is about not manufacturing edge that was never there.

- **Fills cross the spread**, both ways. Long legs pay the ask, short legs receive
  the bid, opening and closing. Mid pricing is not offered — SPY calls average a
  2.45% relative spread and a vertical crosses it four times a round trip.
- **Decide on bar N, fill on bar N+1.** A signal is computed from a bar's own
  close, and nobody trades at a price they are still waiting to observe.
- **Trades that could not be placed are refused and counted.** META's ladder
  reaches barely 1% above spot on many bars — there is no 0.30-delta call on it.
  A selector that quietly took the nearest strike would invent the trade. Every
  rejection is typed and carries what *was* available.
- **No Sharpe ratio.** One expiry and ~44 sessions cannot support one. The report
  carries its own `caveats` list instead.
- **Expiry settlement is reported separately.** The option tape stops the day
  before expiry, so survivors settle at intrinsic against the stock's expiry-day
  print — the one part of a result the tape never priced.
- **Implied vol and delta return `Option`.** A strike quoting at or below
  intrinsic is *unusable*, not zero-delta, and cannot be selected.

## Layout

| | |
|---|---|
| `data/` | Read-only snapshot access, coverage catalog, chain loading |
| `pricing/` | Black-76, put-call-parity forward, implied vol, delta |
| `model/` | Forecasts, ranking, evaluation, validation gates |
| `execution/` | Which strikes fill, and what crossing the book costs |
| `indicators/` | Incremental SMA, RSI, ATR, VWAP, realized vol — the feature engine's inputs |
| `signal/`, `structure/` | The retired staged pipeline, kept dormant behind `Strategy::Staged` |
| `portfolio/`, `engine/` | Allocation policies, positions, bar loop, exits, settlement |
| `report/` | Summary, trade log, equity curve, rejection census |
| `models/`, `rankers/`, `portfolios/`, `exits/`, `datasets/` | Labeled block registries |

`tests/golden.rs` pins hand-confirmed values against the real snapshot and skips
cleanly when none is present.

See [CLAUDE.md](CLAUDE.md) for the reasoning behind the choices above.

## Related

- [`ib-option-calc`](../ib-option-calc) — the capture side that produces snapshots
- [`ib-option-viz`](../ib-option-viz) — the Streamlit read side; its
  `pricing.py` is the reference this crate's option maths is ported from and
  pinned against
