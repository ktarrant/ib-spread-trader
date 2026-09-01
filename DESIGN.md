# Option Selection Engine

The engine is a pipeline of four blocks run over a dataset:

```
PriceModel  ->  Ranker  ->  PortfolioModel  ->  ExitModel        (over a DataSet)
 (forecast)     (score)      (size & fund)       (when to close)
```

**Design principle — labeled, validated blocks.** Each block is a *named,
self-contained configuration* that is validated in isolation and then **labeled**
so it can be reused and composed. A backtest is then just a labeled tuple:

```
Backtest = (PriceModel, Ranker, PortfolioModel, ExitModel, DataSet)
```

This composition is implemented. Labels resolve strict TOML blocks under
`models/`, `rankers/`, `portfolios/`, `exits/`, and `datasets/`; the same fields
can still be supplied inline for one-off runs. JSON validation records live beside
model and ranker configs, making each label's evidence auditable.

---

## 1. Price-Forecasting Model

**Today.** A pluggable `PriceModel` (currently `LognormalDriftModel`) turns the
available features — trend (fast/slow SMA gap), VWAP distance, ATR, realized vol,
ATM IV, put/call skew, DTE, time-of-day, days-to/since-earnings — into a
probability distribution over the underlying at a horizon. It is defined by its
feature periods (`[features]`) and its weights (`[model]`: the realized-vol/IV
blend, the drift weights on trend/VWAP/skew, the grid resolution). The trait is
the seam: a richer model drops in without the ranker or portfolio changing.

The validated main model is **mean-reversion** (`trend_weight = -0.5`, ATM-IV vol,
one-week horizon): it beats a random-walk null (CRPS skill +0.013, 57% directional
hit-rate, robust across 9/12 symbols under leave-one-symbol-out). `driftless`
(`trend_weight = 0`) is the honest null it must beat.

**Implemented.** The forecast horizon is owned and exposed by `PriceModel`, so a
model label is `{features, weights, horizon}` and is evaluated at that exact
horizon. `scripts/validate_model.py` writes `models/<label>.json`; a model is
labeled only after it clears the forecast eval on a stated dataset:
- **CRPS skill > 0** vs the random-walk null (the null-beating gate), and
- calibration (PIT, 50/80/90 coverage) near nominal, and
- a directional hit-rate materially above 50%, **and**
- consistency under **leave-one-symbol-out** (not one or two lucky symbols).

The label carries the validation record (metrics + dataset + date), so a labeled
model is a claim you can audit, not just a name. Trade P&L is **not** part of model
validation — the forecast is judged as a forecaster.

---

## 2. Ranker

**Today.** The ranker enumerates every vertical (debit and credit, both rights)
and naked long the ladder supplies, marks each to model against the forecast
distribution, and returns the **single best** candidate by its metric. Two metrics
exist: `expected_value` (E[P/L] ÷ max-loss, the default) and `prob_profit`. It
applies floors (`min_score`, `min_expected_pl`) and liquidity/loss caps
(`[candidates]`). The whole cascade — enumerated → priceable → liquid →
within-loss → positive-EV → above-min-score → chosen — is reported as a funnel.

The ranker returns **one** best candidate per symbol-bar, not a slate the
portfolio manager sifts. The score already
**models the exit**: it caps each candidate's payoff at the forecast-target price,
so it scores a trade the way the exit will manage it. Ranker scoring and exit
target quantiles are separate fields; shipped labels deliberately set both to
0.85. The ranker owns metric, thresholds, scoring quantile, and candidate
constraints; horizon belongs to the model.

**Implemented — labeled, validated ranking systems.** The ranker is judged by
whether its score *orders trades correctly*, which is a different question from
whether the forecast is good. The **scorer-discrimination eval** already measures
this (Spearman rank correlation of score vs realized P/L, and score-decile
monotonicity, via forward-priced counterfactual candidates). A labeled Ranker
clears a scorer gate (e.g. Spearman > 0 and roughly monotone deciles) on a stated
model + dataset. A Ranker label is only meaningful *paired with* a model — it
scores that model's distribution. `scripts/validate_ranker.py` writes this paired
record under `rankers/<label>.json`.

---

## 3. Portfolio Model

**Today.** A pluggable `PortfolioModel` selects lot counts after the engine orders
candidates best-score-first. The default is a **single shared account**
(`initial_cash = $20k`) that all symbols compete for, subject to:
- a **risk cap** per position (`risk_per_position` scaled by score, up to
  `max_risk_per_position`),
- a **lot cap** (`max_lots`, the stand-in for a market-impact model),
- a **per-symbol cap** (≤1 open position per symbol — diversification), and
- an **account-wide** position cap.

`account = "per_symbol"` opts back into an independent account per symbol.

**Correction to the old doc.** The per-symbol and account-wide caps were missing
from the old description; they are as load-bearing as the risk/lot caps.

Two labeled implementations prove the seam: `shared_20k` preserves score-weighted
sizing and `equal_weight_1` funds one lot per accepted candidate. A portfolio
label is `{account model, sizing rule, caps}` and comparisons include return per
unit of drawdown alongside profit factor.

---

## 4. Exit Model

**Today, enumerated.** Exits are checked every bar, OR'd, first-to-fire wins, in
this priority. Each is configurable and several are **scoped by structure**:

| Rule | Applies to | Fires when | Config |
|---|---|---|---|
| Profit target | any | gross P/L ≥ pct of max profit | `profit_target_pct` |
| Forecast target | any | underlying reaches the favorable forecast quantile | `forecast_target_quantile` |
| Past short strike | **debit spreads** | underlying crosses the short strike (take-profit) | `underlying_past_short_strike` |
| Stop loss | **debit** (incl. naked) | gross P/L ≤ −pct of debit paid | `stop_loss_pct` |
| Live-POP give-up | **debit spreads** | re-scored probability of profit < floor | `live_pop_floor` |
| Trailing stop | **naked longs** | gross P/L gives back pct of its peak | `naked_trailing_stop_pct` |
| Momentum exit | **naked longs** | the trend flips against the position | `naked_momentum_exit` |
| Forecast horizon | any | held as long as the forecast horizon | `close_after_forecast_horizon` |
| Time decay | any | pct of time-to-expiry elapsed | `max_time_pct` |
| Before earnings | any | the session before an earnings event | `close_before_earnings` |
| Before expiry | any | within N days of expiry (hard cutoff) | `min_days_before_expiry` |

**Corrections to the old doc.** These are per-**structure**, not per-strategy, and
**configurable, not automatic** — a rule only fires if its structure matches *and*
the config enables it. Specifically: **naked longs do not use a trailing stop by
default** — the default naked exit is the forecast target (+ forecast horizon); the
trailing stop and the momentum exit are *variants under comparison*
(`exit_pop_trailing.toml`, `exit_momentum.toml`). And the forecast-horizon exit is
**not** credit-only; it applies to any structure (it is merely the main cap credits
rely on).

**Implemented — labeled Exit blocks.** `base`, `pop_trailing`, and `momentum` are
coherent bundles under `exits/`. Each owns its forecast-target quantile, separate
from the ranker's scoring cap, and an inline `[exit]` remains available.

---

## 5. Backtest

**Today.** A backtest resolves five label-or-inline blocks, simulates them
bar-by-bar, and reports both trade results and forecast quality.
`scripts/compare_strategies.py` runs single-axis label variants side by side.

**Correction to the old doc — how performance is judged.** Performance is **not**
judged primarily on trade P/L. On the current dataset (one snapshot, one expiry,
~4–157 trades) trade P/L is statistical noise, while the ~6,000 per-bar forecasts
have real power. So a backtest is judged **forecast-first**: CRPS skill vs the
null, calibration, directional hit-rate — with trade P/L (return, drawdown, profit
factor) as a *guardrail*, not the headline. "Best performance" means best forecast
skill that also survives on a realistic account, not highest backtest return.

**Implemented — a backtest is a labeled composition.** A backtest is literally
`(PriceModel, Ranker, PortfolioModel, ExitModel, DataSet)` — five labels rather
than one inlined TOML. Benefits: a validated block is reused verbatim across
backtests; a comparison names exactly which one axis changed; and results are
attributable ("this loss is the ExitModel, the forecast label is unchanged"). The
**DataSet** is itself a label — today there is exactly one (a single snapshot /
single expiry), which is the binding limit on how far any validation generalizes;
capturing more expiries is the highest-leverage unlock and would let a DataSet
label mean "walk-forward across dates."

---

## Implementation status

Items 1–6 are complete: model-owned horizon, model and ranker validation
registries, labeled exits, pluggable portfolio allocation, and five-label backtest
composition. The remaining constraint is external: `DataSet = "2026-08-21"` is
fixed to one snapshot/expiry until the capture side provides more. Once it does,
the current per-symbol robustness check generalizes to walk-forward validation
across dataset dates. This repository does not modify the capture tool.
