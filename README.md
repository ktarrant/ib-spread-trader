# ib-spread-trader

Backtests modular option-spread strategies against captured Interactive Brokers
history. Built to validate algorithms before they trade live through the IB API;
**it places no orders, live or paper, and opens its data read-only.**

A strategy is three composable stages, named in a TOML file:

```
Signal              ->  Structure            ->  Selector
RSI(14) crosses 30      bull call spread         which strikes, at what cost
```

Each stage is a Rust trait, so parameters change without recompiling and new
logic is a small impl.

## Quick start

Needs a capture snapshot from [`ib-option-calc`](../ib-option-calc) in
`~/Documents/Options Database` (or set `$IB_SNAPSHOT`).

```sh
cargo test
cargo run -- catalog                                       # what is backtestable
cargo run -- chain --symbol SPY --expiry 2026-08-21 \
                   --at 2026-08-19T15:00:00+00:00          # one resolved ladder
cargo run -- backtest --config strategies/rsi_bull_call.toml --out out/
```

Every subcommand prints JSON to stdout and nothing else, so it pipes into `jq`.
`--out` additionally writes `trades.csv` and `equity.csv`.

## A strategy

```toml
name    = "rsi-oversold-bull-call"
symbols = ["SPY", "QQQ", "NVDA"]
expiry  = "2026-08-21"

[signal]
kind = "rsi_cross"
period = 14
level = 30.0

[structure]
kind = "vertical"
right = "call"          # call + bullish trigger = bull call debit spread
long_delta = 0.50
short_delta = 0.30
quantity = 1

[execution]
delta_tolerance = 0.07  # refuse rather than substitute a strike nobody asked for
max_debit = 1000.0
timing = "next_bar"

[exit]
profit_target_pct = 50.0
stop_loss_pct = 100.0
```

Legs are named by **absolute delta**, which is why one `[structure]` block covers
both directions: for calls strike rises as delta falls, so long-0.50/short-0.30 is
a bull call; for puts strike rises as |delta| rises, so the identical targets give
a bear put.

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
| `indicators/`, `signal/` | Stage one: when to act |
| `structure/` | Stage two: what to build |
| `execution/` | Stage three: which strikes, and what it costs |
| `portfolio/`, `engine/` | Positions, the bar loop, exits, settlement |
| `report/` | Summary, trade log, equity curve, rejection census |

`tests/golden.rs` pins hand-confirmed values against the real snapshot and skips
cleanly when none is present.

See [CLAUDE.md](CLAUDE.md) for the reasoning behind the choices above.

## Related

- [`ib-option-calc`](../ib-option-calc) — the capture side that produces snapshots
- [`ib-option-viz`](../ib-option-viz) — the Streamlit read side; its
  `pricing.py` is the reference this crate's option maths is ported from and
  pinned against
