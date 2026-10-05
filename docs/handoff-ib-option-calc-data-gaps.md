# Handoff to ib-option-calc: data gaps worth closing

*From ib-spread-trader, 2026-10-05. Every figure was measured on the
`ib-option-calc-2026-10-04.sqlite3` snapshot and the `news-llm.sqlite3` cache
beside it. The analysis is `scripts/news_study.py` in ib-spread-trader; its
report is the News Study page in ib-option-viz.*

The backtester and the news study keep hitting the same few holes in the
capture. Each item below gives the gap, the evidence, what it costs
downstream, a suggested fix, and how to check the fix. The first one is
**time-limited**: every day it waits, a day of headline history leaves IB for
good.

| # | Gap | Priority | Clock running? |
|---|---|---|---|
| 1 | Headline history before 10-04 is one 255-row page deep | **Urgent** | Yes: Dow Jones keeps ~3 months, so August starts expiring ~11-03 |
| 2 | The weekly run misses the week's last session | High | No, but each week loses its Friday for good after ~52 days |
| 3 | ETF strike bands are lopsided and too narrow | High | Each week's band is fixed once IB's ~52 days roll |
| 4 | Strikes drop out of the ladder from one week to the next | Medium | Same |
| 5 | Headline sync history is not recorded | Medium | No |
| 6 | Scorer: ambiguous empty windows; backfilled windows never re-scored | Medium | Follows from #1 |
| 7 | 20 of 36 news symbols have no price tape (AMD and MSTR dropped in August) | Medium | Stock bars can be backfilled later (see #9) |
| 8 | The reference capture (VIX, sector ETFs) is not in the snapshot | Medium | Probably not |
| 9 | Underlying history starts 2026-06-01 | Low, but high value | No (to probe) |
| 10 | Earnings: XOM has one event; no forward calendar | Low | No |
| 11 | No expiry-day option bars | Low | Only on expiry day itself |

---

## 1. Headline history is one page deep, and the fix is in `ibapi`'s date format

**Evidence.**
- All 34 015 headlines in the snapshot have `captured_at` 2026-10-04. Rows go
  in with `INSERT OR IGNORE`, so the whole table is a single sync.
- The scorer then ran from 10-04 22:03 to 10-05 03:47 over windows back to
  08-03. Each provider's 255 newest rows reached back only so far:

  | complete from | symbols |
  |---|---|
  | 09-23 → 09-28 | NVDA 09-24, META 09-23, GLD 09-28 |
  | 09-08 → 09-16 | AAPL 09-10, AMZN 09-09, MSFT 09-08, GOOGL 09-16, SPCX 09-15 |
  | 08-17 → 08-27 | AMD 08-27, TSLA 08-24, COIN 08-21, WMT 08-20, BA 08-17, LLY 08-17 |
  | ~08-03 → 08-10 | MSTR, PLTR, UBER, AVGO |
  | July or earlier | XOM, NFLX, GS, JPM, CVX, CAT, COST, most of the ETFs |

- Headlines actually sent to the scorer for NVDA, per week from 08-03:
  0, 0, 1, 18, 62, … 133. META ran ~12 a week in August against 137 in late
  September.
- NVDA is the second-most-covered name, so those empty weeks are the cap, not
  a quiet market.

**Cost downstream.**
- Only 381 of 597 single-name sessions in the scored window are usable.
- Every "no news" comparison made before the coverage mask existed partly
  compared news against headlines that were never captured. That includes the
  engine's sentiment gate, `news_audit.py`'s event study and the first quick
  tests.
- The one news result that looked like a volatility signal (1.31× variance
  after vol-flagged news) fell to 1.21 [0.98, 1.42] once the mask was applied.

**Fix.** `src/events/news.rs` already names the cause:
- `ibapi` sends the request's start and end as `yyyyMMdd HH:mm:ss UTC`, and
  IB expects `yyyy-MM-dd HH:mm:ss.0`.
- The fields are pushed as `OffsetDateTime`s in
  `ibapi-2.11.4/src/news/common/encoders.rs:54-55`
  (`encode_request_historical_news`).
- Push preformatted strings instead, via a `[patch.crates-io]` fork or an
  upstream PR.

Then page backwards per (symbol, provider):
- Set `endDateTime` to the oldest headline already held.
- Repeat until a page returns fewer than 255 rows or reaches 2026-07-01.
- Do the heaviest names first (NVDA, META, GLD, GOOGL, AAPL, AMZN), because
  their August is the first to expire.
- At ~25–28 headlines a day per Dow Jones provider for NVDA, a page covers
  ~9–10 days, so August–September is a handful of requests per provider.

**Check.** Ask NVDA / DJ-N with `endDateTime` 2026-08-24. Today its oldest row
is 09-24 13:01. If the fix works, rows from before 08-24 come back. If IB still
returns the newest 255, the format was not the whole story, and that is worth
knowing before building on it.

## 2. The weekly run misses the week's last session

**Evidence.**
- The main run `2026-10-01T20:30 → 2026-10-04T03:26` started Thursday at 16:30
  ET and took 55 hours (30-minute bars, 15 symbols).
- Its option and stock bars all end at **10-01 19:55**. Friday 10-02, a
  payrolls day, is missing for every symbol except SPCX, which had its own run.
- Requests seem to end where the run was planned, not where each request is
  sent.

**Cost.**
- Fridays carry a disproportionate share of the events this project studies:
  payroll sessions run 4.1× an ETF's normal variance, and monthly expiries fall
  on Fridays.
- A missing Friday is filled only by the next week's run, and only while it is
  inside IB's ~52-day window.

**Fix.** Start the weekly run after Friday's close, or end each request at
send time. Or add a short top-up of the last session before the snapshot is
taken.

**Check.** The next snapshot's last option and stock bar is the Friday 15:45 /
15:55 ET bar.

## 3. ETF strike bands: lopsided this week, too narrow always

**Evidence.** The Nov-20 chain in the latest capture (bars from 09-24):

| | strikes | spot 10-01 | band against spot |
|---|---|---|---|
| SPY | 730 – 779 (30) | 764.0 | −4.5 % … +2.0 % |
| QQQ | 665 – 748 (30) | 742.1 | −10.4 % … **+0.8 %** |
| IWM | 276 – 305 (30) | 279.1 | **−1.1 %** … +9.3 % |

QQQ has almost no out-of-the-money calls and IWM almost no out-of-the-money
puts. Both bands look centred where the ETF stood in mid-September (QQQ ~705,
IWM ~285–290), before QQQ rallied ~5 % and IWM fell ~3 %. The Oct-16 chain is wide
(SPY −11 % … +3.4 %), but it holds both a 15- and a 30-minute pass.

**Cost.**
- The one structure that has kept the index put premium after costs, a skewed
  condor with short puts 4.5–6.5 % out and short calls 1–3 % out, cannot be
  placed on SPY at all. SPY's band ends near −5 %, so there is no 6.5 % put.
- This week it couldn't be placed on IWM or QQQ either.
- `fixed_unplaceable` counts these bars downstream; a short ladder reads as
  "no signal" otherwise.

**Fix.**
- For SPY, QQQ and IWM, set a fixed minimum band around the *current* spot,
  e.g. −8 % … +5 %, whatever σ says. These chains are liquid and the extra
  strikes are cheap.
- Check what the band is centred on when the run starts after a big move
  (`spot_range`, `band_since`, and `sigma_source`, which fell back to
  `realized` for SPCX this week).

**Check.** In the next capture, each ETF's Nov and Dec chains quote strikes
from ≥ 7 % below spot to ≥ 4 % above it on the last bar.

## 4. Strikes drop out of the ladder week to week

**Evidence.**
- On the 10-03 snapshot, a bar quotes a median **7 of 13** registered strikes
  within ±0.5σ√T of the forward (p10: 4).
- The 30- and 15-minute passes took different strike sets for the same chain
  (IWM 250–310 against 265–305).
- A strike captured one week and left out the next has bars up to that week
  and none after.

**Cost.**
- The backtester has to price missing strikes off the bar's own smile.
  Bracketed estimates are good (median error 0.14–0.27 of a half-spread); past
  the quoted range they are not (p90 3.5–61 half-spreads).
- Positions opened on one ladder go unquoted across the seam and exit at
  intrinsic.

**Fix.** Make the ladder sticky: keep refreshing every strike previously
captured within ±1σ√T of the current spot until expiry, ahead of new far
wings.

**Check.** Rerun the median quoted-of-registered count on the next snapshot:
it should approach 13 of 13 near the money.

## 5. Record each headline sync

**Evidence.** Completeness is currently inferred from the `captured_at`
batches in the `events` table (`news_profile.complete_from` in
ib-spread-trader). With a single batch there is nothing to infer from, and an
outage longer than ~9 days on the heaviest names would reopen holes silently.

**Fix.** Add a `news_sync_log` row per (symbol, provider, sync): `synced_at`,
`returned`, `oldest`, `newest`, `capped`. Store a page's actual reach, not the
range asked for. This is the headline equivalent of `capture_coverage`.

**Check.** A reader can state, for any (symbol, time), whether the record is
complete, without heuristics.

## 6. The scorer (the one writable corner here)

- **Mark every window.** About one window a day has no call, because the noise
  filter dropped all of its headlines. Downstream that is indistinguishable
  from a failed or skipped call, so a failure inside a scored stretch reads as
  "quiet". A status row per window (`scored`, `all filtered`, `failed`) would
  remove the ambiguity.
- **Re-score windows that gain headlines.** After #1's backfill, old windows
  hold headlines their call never saw. The scorer only scores windows the last
  sync closed, and `(window_end, chunk, model, prompt_version)` is the key. So
  re-scoring a window needs replace semantics, or a `PROMPT_VERSION` bump for
  the backfilled range.
  - Readers take only the newest model and prompt, so a partial bump would hide
    the un-bumped windows.
  - Budget: ~600 calls cover 08-03 → 10-02, about two usage windows.

## 7. Price tape for every news symbol

**Evidence.**
- 36 symbols get headlines and scores. Only 16 have a current price tape:
  AAPL, AMZN, BA, COIN, GS, IWM, META, NFLX, NVDA, PLTR, QQQ, SPCX, SPY, TSLA,
  UBER, XOM.
- AMD and MSTR were in the August chain and stop on 08-27, both stock and
  options, yet still draw 167 and 119 scored effects.
- GOOGL, MSFT, LLY, WMT, AVGO, CAT, COST, CVX, JPM, HIMS, UNH and the
  sector/commodity ETFs have news and no bars.

**Cost.** Their news can't be tested against anything. They account for 1 767 of
the 4 128 scored effects (43 %).

**Fix.** Add underlying-only 5-minute bars for every news-cohort symbol
(options optional), and confirm that dropping AMD and MSTR from the option
universe was intended.

## 8. The reference capture is missing

**Evidence.** The 10-04 snapshot holds only `stock` and `option` contracts: no
indices (the VIX complex) and none of the sector or cross-asset ETFs. The run
log shows no underlying-only reference run after the 10-01 weekly run, which
completed 10-04 03:26; the next run was SPCX alone.

**Fix.** Check why the reference capture did not follow the completed weekly
run. The macro and vol work would use VIX directly.

## 9. Deeper underlying history (probe first)

**Evidence.**
- Five-minute stock bars start 2026-06-01 and daily bars 2026-05-15.
- The macro study therefore has 3 FOMC meetings, 4 CPI and 4 payrolls, and the
  payroll result (4.1× ETF variance) rests on those 4.

**Ask.**
- Find out how far back IB serves *stock* 5-minute bars: the ~52-day limit is
  for options.
- If it's a year or more, do a one-off backfill for the universe and SPY, QQQ,
  IWM and VIX. That multiplies every event sample the calendar gives,
  including ~8 FOMC meetings a year, and the stock features need no option
  history.

## 10. Earnings data

- **XOM has one earnings row** (2026-07-31), against dozens for the others. It
  looks like an `events-sync` gap for that filer.
- **No forward earnings calendar.**
  - WSH is not entitled, and EDGAR knows a release only once it is filed.
  - The LLM's `future_events` is not a substitute: its 17 earnings mentions are
    nearly all same-day or same-week, and NVDA's 08-26 print was never
    announced.
  - Live trading needs a forward calendar from somewhere: a WSH entitlement,
    or another source.

## 11. Expiry-day option bars

**Evidence.** The Aug-21 chain's last option bar is 08-20 20:00. The tape
stops the day before expiry, so the backtester settles anything still open at
intrinsic against the stock's print, and it closes everything 3 days early to
avoid that.

**Ask, if possible.** Capture expiring chains after the close on expiry day,
while IB still serves them. It's low value for now and only possible on the
day.

---

## Not gaps, so not worth chasing

- **The macro calendar** (FOMC list plus FRED) is complete and is used as is:
  31 releases inside the tape, with times.
- The empty `quotes` table, the zero volume on bid/ask rows and the lack of
  historical greeks are known and handled.
- **15-minute option bars from 2026-10-08** are already decided. Downstream
  reads both sizes and folds where chains differ; the next snapshot will show
  whether the switch took.
