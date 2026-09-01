//! The probabilistic price-target engine and the vertical ranker.
//!
//! An alternative decision path to the `signal -> structure -> selector`
//! pipeline. Instead of a directional trigger picking one structure at a fixed
//! delta, this path:
//!
//! 1. extracts a [`Features`] vector from each option bar ([`features`]),
//! 2. turns it into a probability distribution over the underlying at a scoring
//!    horizon (a price model, added in a later phase),
//! 3. enumerates the verticals the ladder can actually supply and ranks them by
//!    expected P/L per unit of risk (added in a later phase).
//!
//! Everything here is fed one option bar at a time, in order — the same
//! discipline the signals keep, so look-ahead stays structurally impossible.

pub mod candidates;
pub mod distribution;
pub mod eval;
pub mod features;
pub mod price_model;
pub mod ranker;
pub mod validation;

pub use candidates::{Candidate, CandidateConfig, RankingFunnel, enumerate_verticals};
pub use distribution::PriceDistribution;
pub use eval::{
    DecileStat, ForecastRecord, ModelEval, ScorerEval, ScorerRecord, decile_monotonicity,
};
pub use features::{EarningsDistance, FeatureConfig, FeatureEngine, Features};
pub use price_model::{LognormalDriftModel, LognormalModelConfig, PriceModel};
pub use ranker::{
    RankMetric, RankedChoice, RankingConfig, expected_pl_and_score, prob_profit_of, rank,
    ranking_key,
};
pub use validation::{RankerValidationRecord, ValidationRecord};
