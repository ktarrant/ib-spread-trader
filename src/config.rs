//! Strategy configuration in TOML.
//!
//! Ranked strategies compose five label-or-inline blocks: price model, ranker,
//! portfolio model, exit model, and dataset. A parameter sweep needs no
//! recompile; genuinely new logic is still a trait implementation.
//!
//! **Every field carries `#[serde(default)]`.** Without it a partial config
//! fails to deserialize, which is a live papercut in the sibling `ib-option-calc`
//! and not worth reproducing.

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::domain::OptionRight;
use crate::engine::{
    ExitRules, RankedStrategy, RsiZone, RunConfig, StagedStrategy, Strategy, Timing,
};
use crate::execution::{Costs, DeltaSelector};
use crate::model::{
    CandidateConfig, FeatureConfig, FeatureEngine, LognormalDriftModel, LognormalModelConfig,
    RankMetric, RankingConfig,
};
use crate::portfolio::{EqualWeightSizer, PortfolioModelKind, ScoreWeightedSizer};
use crate::signal::{
    ContractionMeasure, RsiCross, RsiTrendFiltered, SmaCross, VolatilityContractionBreakout,
    VolatilityContractionParams,
};
use crate::structure::Vertical;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyConfig {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default = "default_expiry")]
    pub expiry: NaiveDate,
    /// A named dataset under `datasets/`, or an inline `[dataset]` block.
    #[serde(default)]
    pub dataset: Option<DatasetBlock>,
    #[serde(default = "default_cash")]
    pub initial_cash: f64,
    /// Which decision engine to run. Absent means the staged pipeline, so every
    /// existing config keeps working untouched.
    #[serde(default)]
    pub mode: StrategyMode,
    /// Legacy inline account selection. Named/inline `portfolio` blocks supersede
    /// this; shared is the default when no portfolio block is present.
    #[serde(default)]
    pub account: AccountMode,
    #[serde(default)]
    pub signal: SignalConfig,
    #[serde(default)]
    pub structure: StructureConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    /// A named bundle under `exits/`, or an inline `[exit]` table.
    #[serde(default)]
    pub exit: ExitBlock,
    #[serde(default)]
    pub costs: CostsConfig,
    /// Legacy inline ranked-mode sections. A model/ranker label supersedes the
    /// corresponding fields while preserving them for one-off configs.
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default)]
    pub model: ModelBlock,
    #[serde(default)]
    pub candidates: CandidatesConfig,
    #[serde(default)]
    pub ranking: RankingSectionConfig,
    /// A named ranker bundle under `rankers/`, or an inline `[ranker]` block.
    #[serde(default)]
    pub ranker: Option<RankerBlock>,
    /// Legacy inline score-weighted sizing; named/inline `portfolio` supersedes it.
    #[serde(default)]
    pub sizing: Option<SizingConfig>,
    /// A named portfolio bundle under `portfolios/`, or an inline `[portfolio]`
    /// block. When absent, the legacy account/sizing fields preserve behavior.
    #[serde(default)]
    pub portfolio: Option<PortfolioBlock>,
}

/// Which decision engine a config selects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyMode {
    /// The `signal -> structure -> selector` pipeline.
    #[default]
    Staged,
    /// The price-target engine: features -> distribution -> ranked verticals.
    Ranked,
}

/// Whether the backtest runs one account per symbol or one shared across all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountMode {
    /// Each symbol runs against its own `initial_cash`.
    PerSymbol,
    /// All symbols share one `initial_cash`, competing for it concurrently.
    /// The default: one realistic limited-capital account is the honest basis
    /// for comparing strategies, not the sum of N independent per-symbol runs.
    #[default]
    Shared,
}

fn default_name() -> String {
    "unnamed".to_string()
}
fn default_cash() -> f64 {
    // A single shared account, so a realistic limited-capital figure rather than
    // a per-symbol sleeve.
    20_000.0
}
fn default_expiry() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid sentinel date")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SignalConfig {
    /// Fires when RSI crosses `level`. Up reads bullish, down bearish.
    RsiCross {
        #[serde(default = "default_rsi_period")]
        period: usize,
        #[serde(default = "default_rsi_level")]
        level: f64,
    },
    SmaCross {
        #[serde(default = "default_fast")]
        fast: usize,
        #[serde(default = "default_slow")]
        slow: usize,
    },
    /// RSI crossing a level, gated by a trend filter. Only fires the oversold
    /// bounce when the broader trend agrees — the classic institutional filter.
    RsiTrendFiltered {
        #[serde(default = "default_rsi_period")]
        period: usize,
        #[serde(default = "default_rsi_level")]
        level: f64,
        /// Single SMA: price must be above (bullish) or below (bearish) this SMA.
        /// Mutually exclusive with `trend_fast`/`trend_slow`.
        #[serde(default)]
        trend_period: Option<usize>,
        /// Fast SMA for a dual-SMA trend gate. Requires `trend_slow`.
        #[serde(default)]
        trend_fast: Option<usize>,
        /// Slow SMA for a dual-SMA trend gate. Requires `trend_fast`.
        #[serde(default)]
        trend_slow: Option<usize>,
    },
    /// A low-volatility regime on the previous bar, followed by a close above
    /// prior highs on elevated, time-of-day-normalised underlying volume.
    VolatilityContractionBreakout {
        #[serde(default)]
        measure: ContractionMeasure,
        #[serde(default = "default_volatility_period")]
        volatility_period: usize,
        #[serde(default = "default_contraction_lookback")]
        contraction_lookback: usize,
        #[serde(default = "default_contraction_percentile")]
        contraction_percentile: f64,
        #[serde(default = "default_breakout_period")]
        breakout_period: usize,
        #[serde(default = "default_volume_lookback_sessions")]
        volume_lookback_sessions: usize,
        #[serde(default = "default_min_volume_ratio")]
        min_volume_ratio: f64,
        #[serde(default = "default_max_atm_iv_increase")]
        max_atm_iv_increase: f64,
    },
}

fn default_rsi_period() -> usize {
    14
}
fn default_rsi_level() -> f64 {
    30.0
}
fn default_fast() -> usize {
    5
}
fn default_slow() -> usize {
    20
}
fn default_volatility_period() -> usize {
    VolatilityContractionParams::default().volatility_period
}
fn default_contraction_lookback() -> usize {
    VolatilityContractionParams::default().contraction_lookback
}
fn default_contraction_percentile() -> f64 {
    VolatilityContractionParams::default().contraction_percentile
}
fn default_breakout_period() -> usize {
    VolatilityContractionParams::default().breakout_period
}
fn default_volume_lookback_sessions() -> usize {
    VolatilityContractionParams::default().volume_lookback_sessions
}
fn default_min_volume_ratio() -> f64 {
    VolatilityContractionParams::default().min_volume_ratio
}
fn default_max_atm_iv_increase() -> f64 {
    VolatilityContractionParams::default().max_atm_iv_increase
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self::RsiCross {
            period: default_rsi_period(),
            level: default_rsi_level(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StructureConfig {
    /// A debit vertical. `right = "call"` with a bullish trigger is a bull call
    /// spread; `right = "put"` with a bearish one is a bear put spread. The
    /// delta targets are identical either way — see `structure::vertical`.
    Vertical {
        #[serde(default = "default_right")]
        right: OptionRight,
        #[serde(default = "default_long_delta")]
        long_delta: f64,
        #[serde(default = "default_short_delta")]
        short_delta: f64,
        #[serde(default = "default_quantity")]
        quantity: u32,
    },
}

fn default_right() -> OptionRight {
    OptionRight::Call
}
fn default_long_delta() -> f64 {
    0.50
}
fn default_short_delta() -> f64 {
    0.30
}
fn default_quantity() -> u32 {
    1
}

impl Default for StructureConfig {
    fn default() -> Self {
        Self::Vertical {
            right: default_right(),
            long_delta: default_long_delta(),
            short_delta: default_short_delta(),
            quantity: default_quantity(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// How far a strike's |delta| may sit from the target before the trade is
    /// refused. Refusals are counted, never silently substituted.
    #[serde(default = "default_tolerance")]
    pub delta_tolerance: f64,
    /// Cap on capital at risk per spread, in dollars.
    #[serde(default)]
    pub max_debit: Option<f64>,
    #[serde(default)]
    pub timing: Timing,
    /// Account-wide cap on open positions. Absent resolves by account mode: a
    /// per-symbol account caps at 1 (in-or-out), a shared account lifts it so
    /// capital — not a position count — is the binding limit.
    #[serde(default)]
    pub max_open_positions: Option<usize>,
    /// Cap on positions open in a single symbol at once. Absent leaves the
    /// account-wide cap the only limit; a shared account defaults it to 1 (so
    /// capital spreads across names) unless set here.
    #[serde(default)]
    pub max_positions_per_symbol: Option<usize>,
    /// Refuse new entries when fewer than this many calendar days remain before
    /// expiry. Prevents opening positions the before-expiry exit would
    /// immediately close. Set this to `min_days_before_expiry + buffer`.
    #[serde(default = "default_min_entry_days_before_expiry")]
    pub min_entry_days_before_expiry: Option<u32>,
    /// Refuse new entries within this many calendar days of an earnings
    /// Entry-timing band on RSI of the option-bar spot series. Ranked mode only
    /// — the staged path has no feature engine.
    #[serde(default)]
    pub min_entry_rsi: Option<f64>,
    #[serde(default)]
    pub max_entry_rsi: Option<f64>,
    /// Which side of the band opens trades. `inside` (the default) refuses the
    /// extremes and trades the middle; `outside` refuses the middle and trades
    /// only the extremes — a mean-reversion entry that waits for a stretched
    /// tape in either direction rather than acting on every bar.
    #[serde(default)]
    pub entry_rsi_zone: RsiZone,
    /// announcement. Prevents opening positions that would be immediately
    /// closed by `close_before_earnings`.
    #[serde(default)]
    pub earnings_blackout_days: Option<u32>,
}

fn default_tolerance() -> f64 {
    0.07
}
// Default expiry rules for every strategy: close well before the option tape
// stops so a position exits on a real quote rather than settling at intrinsic,
// and refuse entries too close to expiry to open one that the before-expiry exit
// would immediately close. With a single expiry this is a hard blackout; once
// more expiries land, the honest alternative is rolling to the next month.
fn default_min_days_before_expiry() -> Option<u32> {
    Some(3)
}
fn default_min_entry_days_before_expiry() -> Option<u32> {
    Some(8)
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            delta_tolerance: default_tolerance(),
            max_debit: None,
            timing: Timing::default(),
            max_open_positions: None,
            max_positions_per_symbol: None,
            min_entry_days_before_expiry: default_min_entry_days_before_expiry(),
            min_entry_rsi: None,
            max_entry_rsi: None,
            entry_rsi_zone: RsiZone::default(),
            earnings_blackout_days: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitConfig {
    /// Exit-owned favorable forecast quantile. When absent, ranked configs
    /// inherit `ranking.forecast_target_quantile` for backward compatibility.
    #[serde(default)]
    pub forecast_target_quantile: Option<f64>,
    /// Adverse forecast quantile past which to close — the bounds the trade was
    /// underwritten inside, breached. `None` disables it.
    #[serde(default)]
    pub forecast_stop_quantile: Option<f64>,
    /// Percentage of max profit at which to close.
    #[serde(default)]
    pub profit_target_pct: Option<f64>,
    /// Percentage of the debit paid at which to give up.
    #[serde(default)]
    pub stop_loss_pct: Option<f64>,
    /// Close when the underlying crosses the short strike.
    #[serde(default)]
    pub underlying_past_short_strike: bool,
    /// Close after this percentage of the time from entry to expiry has elapsed.
    #[serde(default)]
    pub max_time_pct: Option<f64>,
    /// Close before earnings announcements to avoid the vol event.
    #[serde(default)]
    pub close_before_earnings: bool,
    /// Debit-spread give-up: close when the live probability of profit falls
    /// below this (0..1). Ranked mode only. Applies to debit spreads, not nakeds.
    #[serde(default)]
    pub live_pop_floor: Option<f64>,
    /// Naked-long trailing stop: close when gross P&L gives back this percentage
    /// of its running peak. Applies to naked longs.
    #[serde(default)]
    pub naked_trailing_stop_pct: Option<f64>,
    /// Naked-long momentum exit: close when the trend flips against it. Ranked
    /// mode only.
    #[serde(default)]
    pub naked_momentum_exit: bool,
    /// Close once the position has been open as long as the forecast horizon
    /// (`model.horizon_days`). Ranked mode only — the forecast window is what it
    /// keys off. Holds a credit spread its full forecast duration, then closes it.
    #[serde(default)]
    pub close_after_forecast_horizon: bool,
    /// Close at least this many calendar days before expiration.
    #[serde(default = "default_min_days_before_expiry")]
    pub min_days_before_expiry: Option<u32>,
}

impl Default for ExitConfig {
    fn default() -> Self {
        Self {
            forecast_target_quantile: None,
            forecast_stop_quantile: None,
            profit_target_pct: None,
            stop_loss_pct: None,
            underlying_past_short_strike: false,
            max_time_pct: None,
            close_before_earnings: false,
            live_pop_floor: None,
            naked_trailing_stop_pct: None,
            naked_momentum_exit: false,
            close_after_forecast_horizon: false,
            min_days_before_expiry: default_min_days_before_expiry(),
        }
    }
}

/// A strategy can reuse a named exit bundle or define one inline. TOML itself
/// rejects trying to provide both `exit = "..."` and `[exit]` as a duplicate key.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ExitBlock {
    Label(String),
    Inline(ExitConfig),
}

impl Default for ExitBlock {
    fn default() -> Self {
        Self::Inline(ExitConfig::default())
    }
}

/// A selected block supersedes the legacy fields it replaces, and a superseded
/// field beside it would be **silently ignored** — the same papercut
/// `deny_unknown_fields` exists to prevent. Only the raw text can catch it: once
/// deserialized, a field that was written is indistinguishable from one that
/// took its default. So `load` refuses the pair and names both keys.
///
/// `[model]` is the exception that needs the label test: it is *itself* the
/// inline block, so `[features]` only conflicts with a `model = "label"` string.
fn reject_shadowed_fields(text: &str) -> Result<()> {
    let raw: toml::Table = match toml::from_str(text) {
        // A parse error is the deserializer's to report, with its span.
        Err(_) => return Ok(()),
        Ok(raw) => raw,
    };
    // (block key, only a string label supersedes, the fields it supersedes)
    const BLOCKS: [(&str, bool, &[&str]); 4] = [
        ("model", true, &["features"]),
        ("ranker", false, &["candidates", "ranking"]),
        ("dataset", false, &["symbols", "expiry"]),
        ("portfolio", false, &["account", "initial_cash", "sizing"]),
    ];
    for (block, label_only, shadowed) in BLOCKS {
        let selected = match raw.get(block) {
            None => false,
            Some(value) => !label_only || value.is_str(),
        };
        if !selected {
            continue;
        }
        if let Some(key) = shadowed.iter().find(|key| raw.contains_key(**key)) {
            bail!(
                "`{block}` selects a block, so `{key}` beside it would be ignored; \
                 move it inside the block, or drop `{block}`"
            );
        }
    }
    Ok(())
}

/// A block is written either as a label or as its own inline table.
enum LabelOrInline<T> {
    Label(String),
    Inline(T),
}

/// `#[serde(untagged)]` answers a typo inside an inline block with "data did not
/// match any variant", naming the enum instead of the misspelled key — the exact
/// papercut `deny_unknown_fields` exists to prevent, and one a labeled block file
/// does not have (it deserializes into the inner type directly). Dispatching on
/// the value's own shape — a string is a label, anything else is the block —
/// keeps an inline block's error the inner type's own, which names the field.
fn label_or_inline<'de, D, T>(deserializer: D) -> Result<LabelOrInline<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    match toml::Value::deserialize(deserializer)? {
        toml::Value::String(label) => Ok(LabelOrInline::Label(label)),
        inline => T::deserialize(inline)
            .map(LabelOrInline::Inline)
            .map_err(serde::de::Error::custom),
    }
}

impl<'de> Deserialize<'de> for ExitBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match label_or_inline(deserializer)? {
            LabelOrInline::Label(label) => Self::Label(label),
            LabelOrInline::Inline(inline) => Self::Inline(inline),
        })
    }
}

impl<'de> Deserialize<'de> for PortfolioBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match label_or_inline(deserializer)? {
            LabelOrInline::Label(label) => Self::Label(label),
            LabelOrInline::Inline(inline) => Self::Inline(inline),
        })
    }
}

impl<'de> Deserialize<'de> for ModelBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match label_or_inline(deserializer)? {
            LabelOrInline::Label(label) => Self::Label(label),
            LabelOrInline::Inline(inline) => Self::Inline(inline),
        })
    }
}

impl<'de> Deserialize<'de> for DatasetBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match label_or_inline(deserializer)? {
            LabelOrInline::Label(label) => Self::Label(label),
            LabelOrInline::Inline(inline) => Self::Inline(inline),
        })
    }
}

impl<'de> Deserialize<'de> for RankerBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match label_or_inline(deserializer)? {
            LabelOrInline::Label(label) => Self::Label(label),
            LabelOrInline::Inline(inline) => Self::Inline(inline),
        })
    }
}

fn load_labeled<T: DeserializeOwned>(directory: &str, label: &str) -> Result<T> {
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid {directory} label `{label}`; use letters, digits, '-' or '_'");
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(directory)
        .join(format!("{label}.toml"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("loading {directory} label `{label}` from {}", path.display()))?;
    toml::from_str(&text)
        .with_context(|| format!("parsing {directory} label `{label}` from {}", path.display()))
}

impl ExitBlock {
    fn resolve(&self) -> Result<ExitConfig> {
        match self {
            Self::Label(label) => load_labeled("exits", label),
            Self::Inline(config) => Ok(config.clone()),
        }
    }
}

/// Score-weighted position sizing. Present in a TOML as a `[sizing]` table; its
/// presence turns sizing on, its absence keeps fixed one-lot entries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SizingConfig {
    /// Fraction of total capital to risk on a position scoring at `baseline_score`.
    pub risk_per_position: f64,
    /// Ceiling on any one position's risk, as a fraction of total capital.
    pub max_risk_per_position: f64,
    /// The score that earns exactly `risk_per_position`; higher scores scale up.
    pub baseline_score: f64,
    /// Hard ceiling on contracts per position, independent of capital — bounds
    /// fill size so a cheap structure is not sized past what the ladder absorbs.
    pub max_lots: Option<u32>,
}

impl Default for SizingConfig {
    fn default() -> Self {
        // The shared-account default: a ~ten-position baseline at the floor
        // score scaling toward a quarter-account cap, with a 5-lot fill cap.
        Self {
            risk_per_position: 0.10,
            max_risk_per_position: 0.25,
            baseline_score: 0.02,
            max_lots: Some(5),
        }
    }
}

impl SizingConfig {
    fn to_runtime(&self) -> Result<ScoreWeightedSizer> {
        if !(0.0..=1.0).contains(&self.risk_per_position) || self.risk_per_position == 0.0 {
            bail!(
                "sizing.risk_per_position must be in (0, 1], got {}",
                self.risk_per_position
            );
        }
        if !(self.risk_per_position..=1.0).contains(&self.max_risk_per_position) {
            bail!(
                "sizing.max_risk_per_position must be in [risk_per_position, 1], got {}",
                self.max_risk_per_position
            );
        }
        if self.baseline_score <= 0.0 {
            bail!(
                "sizing.baseline_score must be positive, got {}",
                self.baseline_score
            );
        }
        if self.max_lots == Some(0) {
            bail!("sizing.max_lots must be at least 1 when set");
        }
        Ok(ScoreWeightedSizer {
            risk_per_position: self.risk_per_position,
            max_risk_per_position: self.max_risk_per_position,
            baseline_score: self.baseline_score,
            max_lots: self.max_lots,
        })
    }
}

fn default_equal_lots() -> u32 {
    1
}

/// The allocation policy inside a portfolio bundle.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PortfolioModelConfig {
    ScoreWeighted {
        #[serde(default = "default_risk_per_position")]
        risk_per_position: f64,
        #[serde(default = "default_max_risk_per_position")]
        max_risk_per_position: f64,
        #[serde(default = "default_baseline_score")]
        baseline_score: f64,
        #[serde(default = "default_max_lots")]
        max_lots: Option<u32>,
    },
    EqualWeight {
        #[serde(default = "default_equal_lots")]
        lots: u32,
    },
}

fn default_risk_per_position() -> f64 {
    SizingConfig::default().risk_per_position
}
fn default_max_risk_per_position() -> f64 {
    SizingConfig::default().max_risk_per_position
}
fn default_baseline_score() -> f64 {
    SizingConfig::default().baseline_score
}
fn default_max_lots() -> Option<u32> {
    SizingConfig::default().max_lots
}

impl Default for PortfolioModelConfig {
    fn default() -> Self {
        let sizing = SizingConfig::default();
        Self::ScoreWeighted {
            risk_per_position: sizing.risk_per_position,
            max_risk_per_position: sizing.max_risk_per_position,
            baseline_score: sizing.baseline_score,
            max_lots: sizing.max_lots,
        }
    }
}

impl PortfolioModelConfig {
    fn to_runtime(&self) -> Result<PortfolioModelKind> {
        match self {
            Self::ScoreWeighted {
                risk_per_position,
                max_risk_per_position,
                baseline_score,
                max_lots,
            } => Ok(PortfolioModelKind::ScoreWeighted(
                SizingConfig {
                    risk_per_position: *risk_per_position,
                    max_risk_per_position: *max_risk_per_position,
                    baseline_score: *baseline_score,
                    max_lots: *max_lots,
                }
                .to_runtime()?,
            )),
            Self::EqualWeight { lots } => {
                if *lots == 0 {
                    bail!("portfolio.model.lots must be at least 1");
                }
                Ok(PortfolioModelKind::EqualWeight(EqualWeightSizer {
                    lots: *lots,
                }))
            }
        }
    }
}

/// Account policy and allocation model as one reusable portfolio block.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortfolioConfig {
    pub account: AccountMode,
    pub initial_cash: f64,
    pub max_open_positions: Option<usize>,
    pub max_positions_per_symbol: Option<usize>,
    pub model: PortfolioModelConfig,
}

impl Default for PortfolioConfig {
    fn default() -> Self {
        Self {
            account: AccountMode::Shared,
            initial_cash: default_cash(),
            max_open_positions: None,
            max_positions_per_symbol: None,
            model: PortfolioModelConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PortfolioBlock {
    Label(String),
    Inline(PortfolioConfig),
}

impl PortfolioBlock {
    fn resolve(&self) -> Result<PortfolioConfig> {
        match self {
            Self::Label(label) => load_labeled("portfolios", label),
            Self::Inline(config) => Ok(config.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedPortfolio {
    account: AccountMode,
    initial_cash: f64,
    max_open_positions: usize,
    max_positions_per_symbol: Option<usize>,
    model: Option<PortfolioModelKind>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CostsConfig {
    #[serde(default = "default_commission")]
    pub commission_per_contract: f64,
}

fn default_commission() -> f64 {
    0.65
}

impl Default for CostsConfig {
    fn default() -> Self {
        Self {
            commission_per_contract: default_commission(),
        }
    }
}

/// `[features]` — the indicators feeding the price model. Container-level
/// `#[serde(default)]` lets a partial section fill the rest from `Default`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeaturesConfig {
    pub trend_fast: usize,
    pub trend_slow: usize,
    pub atr_period: usize,
    pub realized_vol_period: usize,
    pub realized_vol_winsor_k: f64,
    pub skew_moneyness: f64,
    /// RSI lookback for the entry-timing gate. The price model ignores it.
    pub rsi_period: usize,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        let d = FeatureConfig::default();
        Self {
            trend_fast: d.trend_fast,
            trend_slow: d.trend_slow,
            atr_period: d.atr_period,
            realized_vol_period: d.realized_vol_period,
            realized_vol_winsor_k: d.realized_vol_winsor_k,
            skew_moneyness: d.skew_moneyness,
            rsi_period: d.rsi_period,
        }
    }
}

impl FeaturesConfig {
    fn to_runtime(self) -> FeatureConfig {
        FeatureConfig {
            trend_fast: self.trend_fast,
            trend_slow: self.trend_slow,
            atr_period: self.atr_period,
            realized_vol_period: self.realized_vol_period,
            realized_vol_winsor_k: self.realized_vol_winsor_k,
            skew_moneyness: self.skew_moneyness,
            rsi_period: self.rsi_period,
        }
    }
}

/// `[model]` — the lognormal drift model's weights.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub horizon_days: f64,
    pub bars_per_year: f64,
    pub realized_vol_weight: f64,
    pub fallback_vol: f64,
    pub trend_weight: f64,
    pub vwap_reversion: f64,
    pub skew_weight: f64,
    pub grid_sigma: f64,
    pub grid_points: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        let d = LognormalModelConfig::default();
        Self {
            horizon_days: d.horizon_days,
            bars_per_year: d.bars_per_year,
            realized_vol_weight: d.realized_vol_weight,
            fallback_vol: d.fallback_vol,
            trend_weight: d.trend_weight,
            vwap_reversion: d.vwap_reversion,
            skew_weight: d.skew_weight,
            grid_sigma: d.grid_sigma,
            grid_points: d.grid_points,
        }
    }
}

impl ModelConfig {
    fn to_runtime(self) -> Result<LognormalModelConfig> {
        if self.horizon_days.is_nan() || self.horizon_days <= 0.0 {
            bail!("`model.horizon_days` must be positive");
        }
        Ok(LognormalModelConfig {
            horizon_days: self.horizon_days,
            bars_per_year: self.bars_per_year,
            realized_vol_weight: self.realized_vol_weight,
            fallback_vol: self.fallback_vol,
            trend_weight: self.trend_weight,
            vwap_reversion: self.vwap_reversion,
            skew_weight: self.skew_weight,
            grid_sigma: self.grid_sigma,
            grid_points: self.grid_points,
        })
    }
}

/// A complete labeled forecasting block owns both feature extraction and model
/// weights/horizon. Legacy inline configs retain `[features]` + `[model]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelBundleConfig {
    pub features: FeaturesConfig,
    pub model: ModelConfig,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ModelBlock {
    Label(String),
    Inline(ModelConfig),
}

impl Default for ModelBlock {
    fn default() -> Self {
        Self::Inline(ModelConfig::default())
    }
}

/// The symbols and expiry that identify the currently available dataset.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatasetConfig {
    pub symbols: Vec<String>,
    pub expiry: NaiveDate,
}

impl Default for DatasetConfig {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            expiry: default_expiry(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DatasetBlock {
    Label(String),
    Inline(DatasetConfig),
}

impl DatasetBlock {
    fn resolve(&self) -> Result<DatasetConfig> {
        match self {
            Self::Label(label) => load_labeled("datasets", label),
            Self::Inline(config) => Ok(config.clone()),
        }
    }
}

/// `[candidates]` — which verticals to enumerate and the liquidity/loss limits.
/// `max_width` is optional so "no cap" stays JSON-serializable (an infinity
/// would not).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CandidatesConfig {
    pub rights: Vec<OptionRight>,
    pub allow_debit: bool,
    pub allow_credit: bool,
    pub allow_naked_long: bool,
    pub min_width: f64,
    pub max_width: Option<f64>,
    pub max_relative_spread: f64,
    pub max_loss: f64,
    /// Band on `max_profit / max_loss`. See `CandidateConfig`.
    pub min_reward_risk: Option<f64>,
    pub max_reward_risk: Option<f64>,
    pub quantity: u32,
}

impl Default for CandidatesConfig {
    fn default() -> Self {
        let d = CandidateConfig::default();
        Self {
            rights: d.rights,
            allow_debit: d.allow_debit,
            allow_credit: d.allow_credit,
            allow_naked_long: d.allow_naked_long,
            min_width: d.min_width,
            // The runtime default is unbounded; expose that as "no cap".
            max_width: None,
            max_relative_spread: d.max_relative_spread,
            max_loss: d.max_loss,
            min_reward_risk: d.min_reward_risk,
            max_reward_risk: d.max_reward_risk,
            quantity: d.quantity,
        }
    }
}

impl CandidatesConfig {
    fn to_runtime(&self) -> Result<CandidateConfig> {
        if self.rights.is_empty() {
            bail!("`candidates.rights` is empty; nothing to enumerate");
        }
        if !self.allow_debit && !self.allow_credit && !self.allow_naked_long {
            bail!("`candidates` allows no structure at all (debit/credit/naked)");
        }
        if self.quantity < 1 {
            bail!("`candidates.quantity` must be at least 1");
        }
        if self.max_loss.is_nan() || self.max_loss <= 0.0 {
            bail!("`candidates.max_loss` must be positive");
        }
        if let (Some(lo), Some(hi)) = (self.min_reward_risk, self.max_reward_risk)
            && lo > hi
        {
            bail!("candidates.min_reward_risk ({lo}) exceeds max_reward_risk ({hi})");
        }
        Ok(CandidateConfig {
            rights: self.rights.clone(),
            allow_debit: self.allow_debit,
            allow_credit: self.allow_credit,
            allow_naked_long: self.allow_naked_long,
            min_width: self.min_width,
            max_width: self.max_width.unwrap_or(f64::INFINITY),
            max_relative_spread: self.max_relative_spread,
            max_loss: self.max_loss,
            min_reward_risk: self.min_reward_risk,
            max_reward_risk: self.max_reward_risk,
            quantity: self.quantity,
        })
    }
}

/// `[ranking]` — the scoring thresholds a trade must clear.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankingSectionConfig {
    /// `expected_value` (default) or `prob_profit`.
    pub metric: RankMetric,
    pub min_score: f64,
    pub min_expected_pl: f64,
    /// Refuse to open a candidate whose model probability of profit is below
    /// this. `None`/absent means no probability gate.
    pub min_prob_profit: Option<f64>,
    /// Close when the underlying reaches this quantile of the entry forecast in
    /// the trade's favour (0.5–1.0). `None`/absent disables the forecast exit.
    pub forecast_target_quantile: Option<f64>,
    /// Evaluation only: candidates per bar to price forward for the scorer test.
    /// `0` (the default) disables it, so normal runs pay nothing.
    pub scorer_sample_per_bar: usize,
}

impl Default for RankingSectionConfig {
    fn default() -> Self {
        let d = RankingConfig::default();
        Self {
            metric: d.metric,
            min_score: d.min_score,
            min_expected_pl: d.min_expected_pl,
            min_prob_profit: d.min_prob_profit,
            forecast_target_quantile: d.forecast_target_quantile,
            scorer_sample_per_bar: d.scorer_sample_per_bar,
        }
    }
}

impl RankingSectionConfig {
    fn to_runtime(self) -> Result<RankingConfig> {
        if let Some(q) = self.forecast_target_quantile
            && !(0.5..=1.0).contains(&q)
        {
            bail!("`ranking.forecast_target_quantile` must be in 0.5..=1.0");
        }
        if let Some(floor) = self.min_prob_profit
            && !(0.0..=1.0).contains(&floor)
        {
            bail!("`ranking.min_prob_profit` must be in 0..=1, got {floor}");
        }
        Ok(RankingConfig {
            metric: self.metric,
            min_score: self.min_score,
            min_expected_pl: self.min_expected_pl,
            min_prob_profit: self.min_prob_profit,
            forecast_target_quantile: self.forecast_target_quantile,
            scorer_sample_per_bar: self.scorer_sample_per_bar,
        })
    }
}

/// Candidate universe plus scoring rule as one reusable ranking block.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankerBundleConfig {
    pub candidates: CandidatesConfig,
    pub ranking: RankingSectionConfig,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RankerBlock {
    Label(String),
    Inline(RankerBundleConfig),
}

impl RankerBlock {
    fn resolve(&self) -> Result<RankerBundleConfig> {
        match self {
            Self::Label(label) => load_labeled("rankers", label),
            Self::Inline(config) => Ok(config.clone()),
        }
    }
}

impl StrategyConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading strategy config {}", path.display()))?;
        reject_shadowed_fields(&text)
            .with_context(|| format!("reading strategy config {}", path.display()))?;
        let mut config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing strategy config {}", path.display()))?;
        // Materialize dataset identity for existing callers and for the report's
        // resolved config, while retaining the label alongside it.
        if let Some(dataset) = &config.dataset {
            let resolved = dataset.resolve()?;
            config.symbols = resolved.symbols;
            config.expiry = resolved.expiry;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let dataset = self.resolved_dataset()?;
        if dataset.symbols.is_empty() {
            bail!("`symbols` is empty; nothing to backtest");
        }
        if dataset.expiry == default_expiry() {
            bail!("an expiry is required, inline or through a dataset label");
        }
        if self.portfolio.is_some() && self.sizing.is_some() {
            bail!("use either `portfolio` or legacy `[sizing]`, not both");
        }
        let portfolio = self.resolved_portfolio()?;
        if portfolio.initial_cash <= 0.0 {
            bail!(
                "portfolio initial_cash must be positive, got {}",
                portfolio.initial_cash
            );
        }
        // Build the stages once up front so a typo fails before any I/O rather
        // than after the first symbol has already run.
        self.strategy()?;
        if let Some(sizing) = &self.sizing {
            if self.mode != StrategyMode::Ranked {
                bail!("`[sizing]` applies only to ranked mode; the staged path has no score");
            }
            sizing.to_runtime()?;
        }
        let exit = self.exit.resolve()?;
        if let Some(q) = exit.forecast_target_quantile
            && !(0.5..=1.0).contains(&q)
        {
            bail!("`exit.forecast_target_quantile` must be in 0.5..=1.0");
        }
        if let Some(q) = exit.forecast_stop_quantile
            && !(0.5..=1.0).contains(&q)
        {
            bail!("`exit.forecast_stop_quantile` must be in 0.5..=1.0");
        }
        if exit.forecast_stop_quantile.is_some() && self.mode != StrategyMode::Ranked {
            bail!("exit.forecast_stop_quantile needs the forecast and applies only to ranked mode");
        }
        // The ranked engine caps the scoring horizon at expiry, so a new entry
        // opened inside `horizon_days` of expiry would be scored on a silently
        // truncated horizon. The entry blackout must therefore be at least as
        // long as the horizon, so every bar that can open a trade gets the full
        // horizon. (A missing blackout leaves no such guarantee at all.)
        // The live-POP and momentum exits read the forecast at exit time, so they
        // only make sense in ranked mode.
        if self.mode != StrategyMode::Ranked
            && (exit.live_pop_floor.is_some() || exit.naked_momentum_exit)
        {
            bail!(
                "exit.live_pop_floor and exit.naked_momentum_exit need the forecast \
                 and apply only to ranked mode"
            );
        }
        if let Some(floor) = exit.live_pop_floor
            && !(0.0..=1.0).contains(&floor)
        {
            bail!("exit.live_pop_floor must be in 0..=1, got {floor}");
        }
        for (name, bound) in [
            ("min_entry_rsi", self.execution.min_entry_rsi),
            ("max_entry_rsi", self.execution.max_entry_rsi),
        ] {
            if let Some(v) = bound
                && !(0.0..=100.0).contains(&v)
            {
                bail!("`execution.{name}` must be in 0..=100, got {v}");
            }
            if bound.is_some() && self.mode != StrategyMode::Ranked {
                bail!("`execution.{name}` needs the feature engine and applies only to ranked mode");
            }
        }
        if let (Some(lo), Some(hi)) = (self.execution.min_entry_rsi, self.execution.max_entry_rsi)
            && lo > hi
        {
            bail!("execution.min_entry_rsi ({lo}) exceeds max_entry_rsi ({hi})");
        }
        if self.mode == StrategyMode::Ranked {
            let model = self.resolved_model()?;
            match self.execution.min_entry_days_before_expiry {
                Some(min_days) if (min_days as f64) < model.model.horizon_days => bail!(
                    "execution.min_entry_days_before_expiry ({}) must be >= model.horizon_days ({}) \
                     so a scored entry always gets the full horizon",
                    min_days,
                    model.model.horizon_days
                ),
                None => bail!(
                    "ranked mode needs execution.min_entry_days_before_expiry set to at least \
                     model.horizon_days ({}), or a late entry is scored on a truncated horizon",
                    model.model.horizon_days
                ),
                _ => {}
            }
        }
        Ok(())
    }

    /// The config with every label replaced by the block it names.
    ///
    /// A report has to describe the run that **happened**, not the shorthand
    /// that produced it. Serializing the config as written would emit
    /// `model = "mean-reversion"` where a reader expects the model's fields, and
    /// — worse — would emit the *default* `[candidates]`/`[ranking]` tables
    /// beside a `ranker` label, so a credit-only run would report itself as
    /// allowing debits. The labels are reported separately by `labels`.
    pub fn resolved(&self) -> Result<Self> {
        let dataset = self.resolved_dataset()?;
        let model = self.resolved_model()?;
        let ranker = self.resolved_ranker()?;
        let portfolio = self.resolved_portfolio()?;
        Ok(Self {
            symbols: dataset.symbols,
            expiry: dataset.expiry,
            dataset: None,
            features: model.features,
            model: ModelBlock::Inline(model.model),
            candidates: ranker.candidates,
            ranking: ranker.ranking,
            ranker: None,
            exit: ExitBlock::Inline(self.exit.resolve()?),
            account: portfolio.account,
            initial_cash: portfolio.initial_cash,
            ..self.clone()
        })
    }

    /// The labels a run composed, for the report to carry beside the resolved
    /// blocks — the one thing resolving throws away.
    pub fn labels(&self) -> BTreeMap<&'static str, String> {
        let mut labels = BTreeMap::new();
        if let ModelBlock::Label(label) = &self.model {
            labels.insert("model", label.clone());
        }
        if let Some(RankerBlock::Label(label)) = &self.ranker {
            labels.insert("ranker", label.clone());
        }
        if let Some(PortfolioBlock::Label(label)) = &self.portfolio {
            labels.insert("portfolio", label.clone());
        }
        if let ExitBlock::Label(label) = &self.exit {
            labels.insert("exit", label.clone());
        }
        if let Some(DatasetBlock::Label(label)) = &self.dataset {
            labels.insert("dataset", label.clone());
        }
        labels
    }

    fn resolved_dataset(&self) -> Result<DatasetConfig> {
        match &self.dataset {
            Some(block) => block.resolve(),
            None => Ok(DatasetConfig {
                symbols: self.symbols.clone(),
                expiry: self.expiry,
            }),
        }
    }

    fn resolved_model(&self) -> Result<ModelBundleConfig> {
        match &self.model {
            ModelBlock::Label(label) => load_labeled("models", label),
            ModelBlock::Inline(model) => Ok(ModelBundleConfig {
                features: self.features,
                model: *model,
            }),
        }
    }

    fn resolved_ranker(&self) -> Result<RankerBundleConfig> {
        match &self.ranker {
            Some(block) => block.resolve(),
            None => Ok(RankerBundleConfig {
                candidates: self.candidates.clone(),
                ranking: self.ranking,
            }),
        }
    }

    fn resolved_portfolio(&self) -> Result<ResolvedPortfolio> {
        if let Some(block) = &self.portfolio {
            let config = block.resolve()?;
            let max_open_positions = config.max_open_positions.unwrap_or(match config.account {
                AccountMode::Shared => 1_000,
                AccountMode::PerSymbol => 1,
            });
            let max_positions_per_symbol = config.max_positions_per_symbol.or(match config.account {
                AccountMode::Shared => Some(1),
                AccountMode::PerSymbol => None,
            });
            return Ok(ResolvedPortfolio {
                account: config.account,
                initial_cash: config.initial_cash,
                max_open_positions,
                max_positions_per_symbol,
                model: Some(config.model.to_runtime()?),
            });
        }

        let model = self
            .sizing
            .as_ref()
            .map(|s| s.to_runtime().map(PortfolioModelKind::ScoreWeighted))
            .transpose()?
            .or_else(|| {
                (self.account == AccountMode::Shared && self.mode == StrategyMode::Ranked).then(|| {
                    PortfolioModelKind::ScoreWeighted(
                        SizingConfig::default()
                            .to_runtime()
                            .expect("valid default sizing"),
                    )
                })
            });
        Ok(ResolvedPortfolio {
            account: self.account,
            initial_cash: self.initial_cash,
            max_open_positions: self.execution.max_open_positions.unwrap_or(match self.account {
                AccountMode::Shared => 1_000,
                AccountMode::PerSymbol => 1,
            }),
            max_positions_per_symbol: self.execution.max_positions_per_symbol.or(match self.account {
                AccountMode::Shared => Some(1),
                AccountMode::PerSymbol => None,
            }),
            model,
        })
    }

    pub fn account_mode(&self) -> Result<AccountMode> {
        Ok(self.resolved_portfolio()?.account)
    }

    /// A **fresh** strategy for the selected mode. Both modes carry indicator
    /// state, so every symbol needs its own — sharing one would leak one symbol's
    /// price history into the next symbol's readings.
    pub fn strategy(&self) -> Result<Strategy> {
        match self.mode {
            StrategyMode::Staged => Ok(Strategy::Staged(self.staged_strategy()?)),
            StrategyMode::Ranked => Ok(Strategy::Ranked(Box::new(self.ranked_strategy()?))),
        }
    }

    fn staged_strategy(&self) -> Result<StagedStrategy> {
        let signal: Box<dyn crate::signal::Signal> = match self.signal {
            SignalConfig::RsiCross { period, level } => Box::new(RsiCross::new(period, level)?),
            SignalConfig::SmaCross { fast, slow } => Box::new(SmaCross::new(fast, slow)?),
            SignalConfig::RsiTrendFiltered {
                period,
                level,
                trend_period,
                trend_fast,
                trend_slow,
            } => match (trend_period, trend_fast, trend_slow) {
                (Some(p), None, None) => {
                    Box::new(RsiTrendFiltered::with_price_sma(period, level, p)?)
                }
                (None, Some(f), Some(s)) => {
                    Box::new(RsiTrendFiltered::with_sma_cross(period, level, f, s)?)
                }
                (None, None, None) => {
                    bail!("rsi_trend_filtered needs either `trend_period` or `trend_fast`+`trend_slow`")
                }
                _ => {
                    bail!("rsi_trend_filtered: use `trend_period` alone or `trend_fast`+`trend_slow` together, not both")
                }
            },
            SignalConfig::VolatilityContractionBreakout {
                measure,
                volatility_period,
                contraction_lookback,
                contraction_percentile,
                breakout_period,
                volume_lookback_sessions,
                min_volume_ratio,
                max_atm_iv_increase,
            } => Box::new(VolatilityContractionBreakout::new(
                VolatilityContractionParams {
                    measure,
                    volatility_period,
                    contraction_lookback,
                    contraction_percentile,
                    breakout_period,
                    volume_lookback_sessions,
                    min_volume_ratio,
                    max_atm_iv_increase,
                },
            )?),
        };
        let structure: Box<dyn crate::structure::Structure> = match self.structure {
            StructureConfig::Vertical {
                right,
                long_delta,
                short_delta,
                quantity,
            } => Box::new(Vertical::new(right, long_delta, short_delta, quantity)?),
        };
        Ok(StagedStrategy {
            name: self.name.clone(),
            signal,
            structure,
            selector: Box::new(DeltaSelector {
                delta_tolerance: self.execution.delta_tolerance,
                max_debit: self.execution.max_debit,
            }),
        })
    }

    fn ranked_strategy(&self) -> Result<RankedStrategy> {
        let model = self.resolved_model()?;
        let ranker = self.resolved_ranker()?;
        Ok(RankedStrategy {
            name: self.name.clone(),
            features: FeatureEngine::new(model.features.to_runtime())?,
            model: Box::new(LognormalDriftModel::new(model.model.to_runtime()?)),
            candidates: ranker.candidates.to_runtime()?,
            ranking: ranker.ranking.to_runtime()?,
        })
    }

    pub fn run_config(&self) -> Result<RunConfig> {
        let exit = self.exit.resolve()?;
        let portfolio = self.resolved_portfolio()?;
        let model = self.resolved_model()?;
        let ranker = self.resolved_ranker()?;
        Ok(RunConfig {
            initial_cash: portfolio.initial_cash,
            costs: Costs {
                commission_per_contract: self.costs.commission_per_contract,
            },
            timing: self.execution.timing,
            exits: ExitRules {
                forecast_target_quantile: exit
                    .forecast_target_quantile
                    .or(ranker.ranking.forecast_target_quantile),
                forecast_stop_quantile: exit.forecast_stop_quantile,
                profit_target_pct: exit.profit_target_pct,
                stop_loss_pct: exit.stop_loss_pct,
                underlying_past_short_strike: exit.underlying_past_short_strike,
                live_pop_floor: exit.live_pop_floor,
                naked_trailing_stop_pct: exit.naked_trailing_stop_pct,
                naked_momentum_exit: exit.naked_momentum_exit,
                // The forecast-horizon exit keys off the model's own horizon.
                forecast_horizon_days: exit
                    .close_after_forecast_horizon
                    .then_some(model.model.horizon_days),
                max_time_pct: exit.max_time_pct,
                close_before_earnings: exit.close_before_earnings,
                min_days_before_expiry: exit.min_days_before_expiry,
            },
            max_open_positions: portfolio.max_open_positions,
            max_positions_per_symbol: portfolio.max_positions_per_symbol,
            min_entry_days_before_expiry: self.execution.min_entry_days_before_expiry,
            earnings_blackout_days: self.execution.earnings_blackout_days,
            min_entry_rsi: self.execution.min_entry_rsi,
            max_entry_rsi: self.execution.max_entry_rsi,
            entry_rsi_zone: self.execution.entry_rsi_zone,
            portfolio_model: portfolio.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
name    = "rsi-oversold-bull-call"
symbols = ["SPY", "QQQ"]
expiry  = "2026-08-21"
initial_cash = 50000.0

[signal]
kind = "rsi_cross"
period = 14
level = 30.0

[structure]
kind = "vertical"
right = "call"
long_delta = 0.50
short_delta = 0.30
quantity = 2

[execution]
delta_tolerance = 0.10
max_debit = 500.0
timing = "next_bar"
max_open_positions = 1

[exit]
profit_target_pct = 50.0
stop_loss_pct = 100.0

[costs]
commission_per_contract = 0.65
"#;

    #[test]
    fn a_full_config_round_trips_into_a_run() {
        let config: StrategyConfig = toml::from_str(FULL).unwrap();
        config.validate().unwrap();

        assert_eq!(config.symbols, ["SPY", "QQQ"]);
        assert_eq!(config.expiry, NaiveDate::from_ymd_opt(2026, 8, 21).unwrap());

        let run = config.run_config().unwrap();
        assert_eq!(run.initial_cash, 50_000.0);
        assert_eq!(run.timing, Timing::NextBar);
        assert_eq!(run.exits.profit_target_pct, Some(50.0));
        assert_eq!(run.max_open_positions, 1);
        assert_eq!(run.costs.commission_per_contract, 0.65);
    }

    /// The papercut this crate exists to not inherit: in `ib-option-calc` a
    /// partial config fails to deserialize because no field carries a default.
    #[test]
    fn a_minimal_config_fills_in_every_default() {
        let config: StrategyConfig =
            toml::from_str("symbols = [\"SPY\"]\nexpiry = \"2026-08-21\"").unwrap();
        config.validate().unwrap();

        // The default is one shared $20k account.
        assert_eq!(config.initial_cash, 20_000.0);
        assert_eq!(config.account, AccountMode::Shared);
        assert!(matches!(
            config.signal,
            SignalConfig::RsiCross { period: 14, level } if level == 30.0
        ));
        assert!(matches!(
            config.structure,
            StructureConfig::Vertical {
                right: OptionRight::Call,
                quantity: 1,
                ..
            }
        ));
        assert_eq!(config.execution.delta_tolerance, 0.07);
        assert_eq!(config.execution.max_debit, None);
        assert_eq!(config.exit.resolve().unwrap().profit_target_pct, None);
    }

    /// Each symbol must get a fresh signal, or one symbol's price history leaks
    /// into the next symbol's indicator readings.
    #[test]
    fn every_call_builds_an_independent_strategy() {
        let config: StrategyConfig = toml::from_str(FULL).unwrap();
        let (Strategy::Staged(a), Strategy::Staged(b)) =
            (config.strategy().unwrap(), config.strategy().unwrap())
        else {
            panic!("the FULL fixture is a staged strategy");
        };
        assert_eq!(a.name, b.name);
        assert!(!std::ptr::eq(
            &*a.signal as *const _,
            &*b.signal as *const _
        ));
    }

    #[test]
    fn a_ranked_config_builds_the_ranked_engine() {
        let src = "\
symbols = [\"SPY\"]
expiry = \"2026-08-21\"
mode = \"ranked\"
[candidates]
rights = [\"call\", \"put\"]
max_loss = 400.0
[model]
horizon_days = 3.0
[ranking]
min_score = 0.2
";
        let config: StrategyConfig = toml::from_str(src).unwrap();
        assert_eq!(config.mode, StrategyMode::Ranked);
        config.validate().unwrap();
        assert!(matches!(config.strategy().unwrap(), Strategy::Ranked(_)));
        // Unset ranked fields still fall back to their defaults.
        assert_eq!(config.candidates.max_relative_spread, 0.5);
        assert!(config.candidates.allow_credit);
    }

    /// The horizon must fit inside the entry blackout, or a late entry is scored
    /// on a horizon the engine silently truncates at expiry.
    #[test]
    fn a_horizon_longer_than_the_entry_blackout_is_refused() {
        // horizon 10 days > the default 8-day blackout: refused.
        let src = "\
symbols = [\"SPY\"]
expiry = \"2026-08-21\"
mode = \"ranked\"
[model]
horizon_days = 10.0
";
        let err = toml::from_str::<StrategyConfig>(src)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("min_entry_days_before_expiry"), "{err}");

        // Widening the blackout to match makes it valid again.
        let ok = format!("{src}[execution]\nmin_entry_days_before_expiry = 11\n");
        toml::from_str::<StrategyConfig>(&ok).unwrap().validate().unwrap();
    }

    #[test]
    fn a_ranked_section_refuses_unknown_and_invalid_fields() {
        // An unknown key inside a section is refused, like the top level.
        let err = toml::from_str::<StrategyConfig>(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n[model]\nhorzon_days=3.0",
        )
        .unwrap_err();
        // The message names the misspelled key, not the block enum: an inline
        // block deserializes through its own type, so it errors like one.
        assert!(err.to_string().contains("horzon_days"), "{err}");

        // A non-positive horizon fails validation before any I/O.
        let bad: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n[model]\nhorizon_days=0.0",
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn a_named_exit_bundle_resolves_and_owns_its_forecast_target() {
        let config: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\nexit=\"base\"\n[ranking]\nforecast_target_quantile=0.70",
        )
        .unwrap();
        config.validate().unwrap();
        let run = config.run_config().unwrap();
        assert_eq!(run.exits.forecast_target_quantile, Some(0.85));
        assert_eq!(run.exits.profit_target_pct, Some(50.0));
    }

    #[test]
    fn an_inline_exit_inherits_the_ranker_quantile_only_when_unset() {
        let config: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n[exit]\nprofit_target_pct=25.0\n[ranking]\nforecast_target_quantile=0.70",
        )
        .unwrap();
        let run = config.run_config().unwrap();
        assert_eq!(run.exits.forecast_target_quantile, Some(0.70));
        assert_eq!(run.exits.profit_target_pct, Some(25.0));
    }

    #[test]
    fn a_label_and_inline_exit_cannot_both_be_set() {
        let err = toml::from_str::<StrategyConfig>(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nexit=\"base\"\n[exit]\nprofit_target_pct=25.0",
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate key `exit`"), "{err}");
    }

    #[test]
    fn an_unknown_exit_label_fails_with_the_label_in_context() {
        let config: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nexit=\"does-not-exist\"",
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("does-not-exist"), "{err}");
    }

    #[test]
    fn a_named_equal_weight_portfolio_selects_the_alternative_model() {
        let config: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\nportfolio=\"equal_weight_1\"",
        )
        .unwrap();
        config.validate().unwrap();
        let run = config.run_config().unwrap();
        assert_eq!(run.initial_cash, 20_000.0);
        assert_eq!(run.max_positions_per_symbol, Some(1));
        assert!(matches!(
            run.portfolio_model,
            Some(PortfolioModelKind::EqualWeight(EqualWeightSizer { lots: 1 }))
        ));
    }

    #[test]
    fn an_inline_portfolio_is_strict_and_validated() {
        let invalid: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n[portfolio]\n[portfolio.model]\nkind=\"equal_weight\"\nlots=0",
        )
        .unwrap();
        assert!(invalid.validate().is_err());

        let typo = toml::from_str::<StrategyConfig>(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\n[portfolio]\ninitial_cahs=20000",
        )
        .unwrap_err();
        assert!(typo.to_string().contains("initial_cahs"), "{typo}");
    }

    #[test]
    fn the_main_strategy_resolves_all_five_labeled_blocks() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("strategies/mean_reversion.toml");
        let config = StrategyConfig::load(path).unwrap();
        assert_eq!(config.symbols.len(), 12);
        assert_eq!(config.expiry, NaiveDate::from_ymd_opt(2026, 8, 21).unwrap());
        assert!(matches!(&config.model, ModelBlock::Label(label) if label == "mean-reversion"));
        assert!(matches!(&config.ranker, Some(RankerBlock::Label(label)) if label == "ev"));
        assert!(matches!(&config.exit, ExitBlock::Label(label) if label == "base"));
        assert!(matches!(&config.portfolio, Some(PortfolioBlock::Label(label)) if label == "shared_20k"));
        assert!(matches!(&config.dataset, Some(DatasetBlock::Label(label)) if label == "2026-08-21"));

        let Strategy::Ranked(ranked) = config.strategy().unwrap() else {
            panic!("the composition is ranked");
        };
        assert_eq!(ranked.model.horizon_days(), 7.0);
        assert_eq!(ranked.ranking.min_score, 0.02);
    }

    #[test]
    fn unknown_model_ranker_and_dataset_labels_fail_with_context() {
        for (field, value) in [
            ("model", "missing-model"),
            ("ranker", "missing-ranker"),
            ("dataset", "missing-dataset"),
        ] {
            let source = if field == "dataset" {
                format!("mode=\"ranked\"\ndataset=\"{value}\"")
            } else {
                format!(
                    "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n{field}=\"{value}\""
                )
            };
            let config: StrategyConfig = toml::from_str(&source).unwrap();
            let err = config.validate().unwrap_err();
            assert!(err.to_string().contains(value), "{field}: {err}");
        }
    }

    #[test]
    fn a_label_refuses_the_legacy_field_it_supersedes() {
        // Each pair would otherwise parse fine, with the inline half silently
        // ignored — a config that reads as one thing and runs as another.
        for source in [
            "mode=\"ranked\"\nmodel=\"mean-reversion\"\n[features]\ntrend_fast=5",
            "mode=\"ranked\"\nranker=\"ev\"\n[ranking]\nmin_score=0.5",
            "mode=\"ranked\"\nranker=\"ev\"\n[candidates]\nmax_loss=100.0",
            "mode=\"ranked\"\ndataset=\"2026-08-21\"\nsymbols=[\"SPY\"]",
            "mode=\"ranked\"\ndataset=\"2026-08-21\"\nexpiry=\"2026-08-21\"",
            "mode=\"ranked\"\nportfolio=\"shared_20k\"\ninitial_cash=50000.0",
            "mode=\"ranked\"\nportfolio=\"shared_20k\"\naccount=\"per_symbol\"",
        ] {
            let err = reject_shadowed_fields(source).unwrap_err();
            assert!(err.to_string().contains("would be ignored"), "{source}: {err}");
        }
    }

    #[test]
    fn an_inline_block_still_pairs_with_its_own_legacy_fields() {
        // `[model]` *is* the inline block, so `[features]` belongs beside it —
        // only the string label form supersedes them. Same for a one-off account.
        reject_shadowed_fields(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\n[features]\ntrend_fast=5\n[model]\nhorizon_days=7.0",
        )
        .unwrap();
        reject_shadowed_fields("initial_cash=50000.0\naccount=\"per_symbol\"\n[sizing]\nmax_lots=3")
            .unwrap();
    }

    #[test]
    fn every_shipped_strategy_resolves_its_labels() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("strategies");
        let mut count = 0;
        for entry in std::fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let config = StrategyConfig::load(&path)
                .unwrap_or_else(|err| panic!("{}: {err:#}", path.display()));
            // A composition that resolves must also build its stages.
            config.strategy().unwrap();
            config.run_config().unwrap();
            count += 1;
        }
        assert!(count >= 6, "expected the shipped variant set, found {count}");
    }

    /// The report's config is a contract with `ib-option-viz`, which reads
    /// `config.model` and `config.exit` as tables and describes the candidate
    /// universe from `config.candidates`. Labels broke both: a string where a
    /// table was expected, and — quieter, so worse — the *default* candidate and
    /// ranking tables sitting beside a `ranker` label, which would have reported
    /// a credit-only run as allowing debits and naked longs.
    #[test]
    fn the_reported_config_describes_the_run_not_the_labels() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("strategies/credit_pop.toml");
        let config = StrategyConfig::load(path).unwrap();
        let resolved = config.resolved().unwrap();

        // Every block is a table a reader can inspect, not a name to look up.
        assert!(matches!(resolved.model, ModelBlock::Inline(_)));
        assert!(matches!(resolved.exit, ExitBlock::Inline(_)));
        assert!(resolved.ranker.is_none() && resolved.dataset.is_none());

        // And it reports the universe that actually ran.
        assert!(!resolved.candidates.allow_debit, "this run is credit-only");
        assert!(!resolved.candidates.allow_naked_long);
        assert!(resolved.candidates.allow_credit);
        assert_eq!(resolved.ranking.metric, RankMetric::ProbProfit);
        assert_eq!(resolved.ranking.min_score, 0.02);
        let ExitBlock::Inline(exit) = &resolved.exit else {
            panic!("resolved to an inline exit");
        };
        assert_eq!(exit.profit_target_pct, Some(50.0));

        // The labels are not lost, just reported separately.
        let labels = config.labels();
        assert_eq!(labels["ranker"], "credit_pop");
        assert_eq!(labels["model"], "mean-reversion");
        assert_eq!(labels["exit"], "base");
    }

    #[test]
    fn a_typo_in_a_stage_name_fails_before_any_io() {
        let err = toml::from_str::<StrategyConfig>(
            "symbols = [\"SPY\"]\nexpiry = \"2026-08-21\"\n[signal]\nkind = \"rsi_crass\"",
        )
        .unwrap_err();
        assert!(err.to_string().contains("rsi_crass"), "{err}");
    }

    /// A misspelled key silently taking its default would be worse than an
    /// error, so unknown fields are refused.
    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let err = toml::from_str::<StrategyConfig>(
            "symbols = [\"SPY\"]\nexpiry = \"2026-08-21\"\nintial_cash = 5.0",
        )
        .unwrap_err();
        assert!(err.to_string().contains("intial_cash"), "{err}");
    }

    #[test]
    fn invalid_parameters_are_caught_by_validate() {
        let no_symbols: StrategyConfig = toml::from_str("expiry = \"2026-08-21\"").unwrap();
        assert!(no_symbols.validate().is_err());

        let inverted: StrategyConfig = toml::from_str(
            "symbols = [\"SPY\"]\nexpiry = \"2026-08-21\"\n[structure]\nkind = \"vertical\"\nlong_delta = 0.3\nshort_delta = 0.5",
        )
        .unwrap();
        let err = inverted.validate().unwrap_err();
        assert!(err.to_string().contains("short_delta"), "{err}");
    }

    #[test]
    fn rsi_trend_filtered_with_price_sma_parses() {
        let config: StrategyConfig = toml::from_str(
            r#"
symbols = ["SPY"]
expiry = "2026-08-21"

[signal]
kind = "rsi_trend_filtered"
period = 14
level = 30.0
trend_period = 50
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(matches!(
            config.signal,
            SignalConfig::RsiTrendFiltered {
                period: 14,
                level,
                trend_period: Some(50),
                trend_fast: None,
                trend_slow: None,
            } if level == 30.0
        ));
    }

    #[test]
    fn rsi_trend_filtered_with_sma_cross_parses() {
        let config: StrategyConfig = toml::from_str(
            r#"
symbols = ["SPY"]
expiry = "2026-08-21"

[signal]
kind = "rsi_trend_filtered"
period = 14
level = 30.0
trend_fast = 20
trend_slow = 50
"#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn rsi_trend_filtered_without_trend_params_fails_validation() {
        let config: StrategyConfig = toml::from_str(
            r#"
symbols = ["SPY"]
expiry = "2026-08-21"

[signal]
kind = "rsi_trend_filtered"
"#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn volatility_contraction_defaults_are_configurable_without_recompiling() {
        let config: StrategyConfig = toml::from_str(
            r#"
symbols = ["SPY"]
expiry = "2026-08-21"

[signal]
kind = "volatility_contraction_breakout"
measure = "realized_volatility"
volatility_period = 30
breakout_period = 15
min_volume_ratio = 1.25
max_atm_iv_increase = 0.05
"#,
        )
        .unwrap();
        config.validate().unwrap();

        assert!(matches!(
            config.signal,
            SignalConfig::VolatilityContractionBreakout {
                measure: ContractionMeasure::RealizedVolatility,
                volatility_period: 30,
                contraction_lookback: 120,
                contraction_percentile,
                breakout_period: 15,
                volume_lookback_sessions: 20,
                min_volume_ratio,
                max_atm_iv_increase,
            } if contraction_percentile == 0.10
                && min_volume_ratio == 1.25
                && max_atm_iv_increase == 0.05
        ));
    }
}
