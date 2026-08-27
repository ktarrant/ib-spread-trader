//! Plain shared types. `FromStr`/`Display` live here so clap can parse them
//! directly from the command line and so storage strings round-trip in one place.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The capture stores this as `C` or `P` in `contracts.right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OptionRight {
    Call,
    Put,
}

impl OptionRight {
    /// The storage string, exactly as `contracts.right` holds it.
    pub fn as_storage(self) -> &'static str {
        match self {
            Self::Call => "C",
            Self::Put => "P",
        }
    }

    pub fn is_call(self) -> bool {
        matches!(self, Self::Call)
    }
}

impl FromStr for OptionRight {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "C" | "CALL" => Ok(Self::Call),
            "P" | "PUT" => Ok(Self::Put),
            other => bail!("unknown option right '{other}'; expected C/call or P/put"),
        }
    }
}

impl fmt::Display for OptionRight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Call => "call",
            Self::Put => "put",
        })
    }
}

/// Trim and upper-case, so a symbol looked up matches a symbol stored.
pub fn normalize_symbol(symbol: &str) -> String {
    symbol.trim().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_round_trip_through_their_storage_string() {
        for right in [OptionRight::Call, OptionRight::Put] {
            assert_eq!(right.as_storage().parse::<OptionRight>().unwrap(), right);
        }
    }

    #[test]
    fn rights_accept_the_spellings_a_person_would_type() {
        for s in ["c", "C", "call", "Call", " CALL "] {
            assert_eq!(s.parse::<OptionRight>().unwrap(), OptionRight::Call);
        }
        for s in ["p", "P", "put", " Put "] {
            assert_eq!(s.parse::<OptionRight>().unwrap(), OptionRight::Put);
        }
        assert!("x".parse::<OptionRight>().is_err());
    }

    #[test]
    fn symbols_normalize_for_lookup() {
        assert_eq!(normalize_symbol("  spy "), "SPY");
        assert_eq!(normalize_symbol("SPY"), "SPY");
    }
}
