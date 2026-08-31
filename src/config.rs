//! Strategy configuration in TOML.
//!
//! The three pipeline stages are named by string and their parameters given
//! inline, so a parameter sweep needs no recompile. Genuinely new logic is still
//! a trait impl — this is composition over built-in blocks, not an interpreter.
//!
//! **Every field carries `#[serde(default)]`.** Without it a partial config
//! fails to deserialize, which is a live papercut in the sibling `ib-option-calc`
//! and not worth reproducing.

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::domain::OptionRight;
use crate::engine::{ExitRules, RankedStrategy, RunConfig, StagedStrategy, Strategy, Timing};
use crate::execution::{Costs, DeltaSelector};
use crate::model::{
    CandidateConfig, FeatureConfig, FeatureEngine, LognormalDriftModel, LognormalModelConfig,
    RankMetric, RankingConfig,
};
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
    pub expiry: NaiveDate,
    #[serde(default = "default_cash")]
    pub initial_cash: f64,
    /// Which decision engine to run. Absent means the staged pipeline, so every
    /// existing config keeps working untouched.
    #[serde(default)]
    pub mode: StrategyMode,
    #[serde(default)]
    pub signal: SignalConfig,
    #[serde(default)]
    pub structure: StructureConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub exit: ExitConfig,
    #[serde(default)]
    pub costs: CostsConfig,
    /// Ranked-mode sections. Ignored in staged mode.
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub candidates: CandidatesConfig,
    #[serde(default)]
    pub ranking: RankingSectionConfig,
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

fn default_name() -> String {
    "unnamed".to_string()
}
fn default_cash() -> f64 {
    100_000.0
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
    #[serde(default = "default_max_open")]
    pub max_open_positions: usize,
    /// Refuse new entries when fewer than this many calendar days remain before
    /// expiry. Prevents opening positions the before-expiry exit would
    /// immediately close. Set this to `min_days_before_expiry + buffer`.
    #[serde(default = "default_min_entry_days_before_expiry")]
    pub min_entry_days_before_expiry: Option<u32>,
    /// Refuse new entries within this many calendar days of an earnings
    /// announcement. Prevents opening positions that would be immediately
    /// closed by `close_before_earnings`.
    #[serde(default)]
    pub earnings_blackout_days: Option<u32>,
}

fn default_tolerance() -> f64 {
    0.07
}
fn default_max_open() -> usize {
    1
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
            max_open_positions: default_max_open(),
            min_entry_days_before_expiry: default_min_entry_days_before_expiry(),
            earnings_blackout_days: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitConfig {
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
    /// Close at least this many calendar days before expiration.
    #[serde(default = "default_min_days_before_expiry")]
    pub min_days_before_expiry: Option<u32>,
}

impl Default for ExitConfig {
    fn default() -> Self {
        Self {
            profit_target_pct: None,
            stop_loss_pct: None,
            underlying_past_short_strike: false,
            max_time_pct: None,
            close_before_earnings: false,
            min_days_before_expiry: default_min_days_before_expiry(),
        }
    }
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
        }
    }
}

/// `[model]` — the lognormal drift model's weights.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
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
    fn to_runtime(self) -> LognormalModelConfig {
        LognormalModelConfig {
            bars_per_year: self.bars_per_year,
            realized_vol_weight: self.realized_vol_weight,
            fallback_vol: self.fallback_vol,
            trend_weight: self.trend_weight,
            vwap_reversion: self.vwap_reversion,
            skew_weight: self.skew_weight,
            grid_sigma: self.grid_sigma,
            grid_points: self.grid_points,
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
    pub min_width: f64,
    pub max_width: Option<f64>,
    pub max_relative_spread: f64,
    pub max_loss: f64,
    pub quantity: u32,
}

impl Default for CandidatesConfig {
    fn default() -> Self {
        let d = CandidateConfig::default();
        Self {
            rights: d.rights,
            allow_debit: d.allow_debit,
            allow_credit: d.allow_credit,
            min_width: d.min_width,
            // The runtime default is unbounded; expose that as "no cap".
            max_width: None,
            max_relative_spread: d.max_relative_spread,
            max_loss: d.max_loss,
            quantity: d.quantity,
        }
    }
}

impl CandidatesConfig {
    fn to_runtime(&self) -> Result<CandidateConfig> {
        if self.rights.is_empty() {
            bail!("`candidates.rights` is empty; nothing to enumerate");
        }
        if !self.allow_debit && !self.allow_credit {
            bail!("`candidates` allows neither debit nor credit verticals");
        }
        if self.quantity < 1 {
            bail!("`candidates.quantity` must be at least 1");
        }
        if self.max_loss.is_nan() || self.max_loss <= 0.0 {
            bail!("`candidates.max_loss` must be positive");
        }
        Ok(CandidateConfig {
            rights: self.rights.clone(),
            allow_debit: self.allow_debit,
            allow_credit: self.allow_credit,
            min_width: self.min_width,
            max_width: self.max_width.unwrap_or(f64::INFINITY),
            max_relative_spread: self.max_relative_spread,
            max_loss: self.max_loss,
            quantity: self.quantity,
        })
    }
}

/// `[ranking]` — the scoring horizon and the thresholds a trade must clear.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankingSectionConfig {
    pub horizon_days: f64,
    /// `expected_value` (default) or `prob_profit`.
    pub metric: RankMetric,
    pub min_score: f64,
    pub min_expected_pl: f64,
    /// Evaluation only: candidates per bar to price forward for the scorer test.
    /// `0` (the default) disables it, so normal runs pay nothing.
    pub scorer_sample_per_bar: usize,
}

impl Default for RankingSectionConfig {
    fn default() -> Self {
        let d = RankingConfig::default();
        Self {
            horizon_days: d.horizon_days,
            metric: d.metric,
            min_score: d.min_score,
            min_expected_pl: d.min_expected_pl,
            scorer_sample_per_bar: d.scorer_sample_per_bar,
        }
    }
}

impl RankingSectionConfig {
    fn to_runtime(self) -> Result<RankingConfig> {
        if self.horizon_days.is_nan() || self.horizon_days <= 0.0 {
            bail!("`ranking.horizon_days` must be positive");
        }
        Ok(RankingConfig {
            horizon_days: self.horizon_days,
            metric: self.metric,
            min_score: self.min_score,
            min_expected_pl: self.min_expected_pl,
            scorer_sample_per_bar: self.scorer_sample_per_bar,
        })
    }
}

impl StrategyConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading strategy config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing strategy config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.symbols.is_empty() {
            bail!("`symbols` is empty; nothing to backtest");
        }
        if self.initial_cash <= 0.0 {
            bail!("`initial_cash` must be positive, got {}", self.initial_cash);
        }
        // Build the stages once up front so a typo fails before any I/O rather
        // than after the first symbol has already run.
        self.strategy()?;
        Ok(())
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
        Ok(RankedStrategy {
            name: self.name.clone(),
            features: FeatureEngine::new(self.features.to_runtime())?,
            model: Box::new(LognormalDriftModel::new(self.model.to_runtime())),
            candidates: self.candidates.to_runtime()?,
            ranking: self.ranking.to_runtime()?,
        })
    }

    pub fn run_config(&self) -> RunConfig {
        RunConfig {
            initial_cash: self.initial_cash,
            costs: Costs {
                commission_per_contract: self.costs.commission_per_contract,
            },
            timing: self.execution.timing,
            exits: ExitRules {
                profit_target_pct: self.exit.profit_target_pct,
                stop_loss_pct: self.exit.stop_loss_pct,
                underlying_past_short_strike: self.exit.underlying_past_short_strike,
                max_time_pct: self.exit.max_time_pct,
                close_before_earnings: self.exit.close_before_earnings,
                min_days_before_expiry: self.exit.min_days_before_expiry,
            },
            max_open_positions: self.execution.max_open_positions,
            min_entry_days_before_expiry: self.execution.min_entry_days_before_expiry,
            earnings_blackout_days: self.execution.earnings_blackout_days,
        }
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

        let run = config.run_config();
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

        assert_eq!(config.initial_cash, 100_000.0);
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
        assert_eq!(config.exit.profit_target_pct, None);
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
[ranking]
horizon_days = 3.0
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

    #[test]
    fn a_ranked_section_refuses_unknown_and_invalid_fields() {
        // An unknown key inside a section is refused, like the top level.
        let err = toml::from_str::<StrategyConfig>(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n[ranking]\nhorzon_days=3.0",
        )
        .unwrap_err();
        assert!(err.to_string().contains("horzon_days"), "{err}");

        // A non-positive horizon fails validation before any I/O.
        let bad: StrategyConfig = toml::from_str(
            "symbols=[\"SPY\"]\nexpiry=\"2026-08-21\"\nmode=\"ranked\"\n[ranking]\nhorizon_days=0.0",
        )
        .unwrap();
        assert!(bad.validate().is_err());
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
