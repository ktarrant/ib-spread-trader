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
use crate::engine::{ExitRules, RunConfig, Strategy, Timing};
use crate::execution::{Costs, DeltaSelector};
use crate::signal::{RsiCross, SmaCross};
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
}

fn default_tolerance() -> f64 {
    0.07
}
fn default_max_open() -> usize {
    1
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            delta_tolerance: default_tolerance(),
            max_debit: None,
            timing: Timing::default(),
            max_open_positions: default_max_open(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitConfig {
    /// Percentage of max profit at which to close.
    #[serde(default)]
    pub profit_target_pct: Option<f64>,
    /// Percentage of the debit paid at which to give up.
    #[serde(default)]
    pub stop_loss_pct: Option<f64>,
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

    /// A **fresh** strategy. Signals carry indicator state, so every symbol
    /// needs its own — sharing one would leak one symbol's price history into
    /// the next symbol's readings.
    pub fn strategy(&self) -> Result<Strategy> {
        let signal: Box<dyn crate::signal::Signal> = match self.signal {
            SignalConfig::RsiCross { period, level } => Box::new(RsiCross::new(period, level)?),
            SignalConfig::SmaCross { fast, slow } => Box::new(SmaCross::new(fast, slow)?),
        };
        let structure: Box<dyn crate::structure::Structure> = match self.structure {
            StructureConfig::Vertical {
                right,
                long_delta,
                short_delta,
                quantity,
            } => Box::new(Vertical::new(right, long_delta, short_delta, quantity)?),
        };
        Ok(Strategy {
            name: self.name.clone(),
            signal,
            structure,
            selector: Box::new(DeltaSelector {
                delta_tolerance: self.execution.delta_tolerance,
                max_debit: self.execution.max_debit,
            }),
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
            },
            max_open_positions: self.execution.max_open_positions,
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
        let a = config.strategy().unwrap();
        let b = config.strategy().unwrap();
        assert_eq!(a.name, b.name);
        assert!(!std::ptr::eq(
            &*a.signal as *const _,
            &*b.signal as *const _
        ));
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
}
