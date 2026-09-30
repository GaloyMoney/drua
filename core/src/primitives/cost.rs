//! Fixed-point USD amount used to account and enforce `max_cost_usd`
//! workflow budgets (see `handoff-workflow-max-cost-usd-2026-09-30.md`).
//!
//! No decimal crate (`rust_decimal`, `bigdecimal`) is in the workspace, and
//! the only ingress for cost data is `f64` (`llm::response::Usage::cost_usd`,
//! itself sourced from a provider-reported JSON number). Rather than add a
//! new dependency for a single money type, amounts are stored as an `i64`
//! count of micro-dollars (1 unit == $0.000001) — sub-cent precision, no
//! binary-float rounding at the accumulation/comparison boundary the budget
//! enforces, and a trivial, allocation-free `Copy` type. Conversion from the
//! provider's `f64` happens exactly once, at the boundary where a settled
//! cost or a configured limit enters the system.
use serde::{Deserialize, Serialize};

/// Amount in micro-dollars (1_000_000 == $1.00).
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MicroUsd(i64);

/// Above this, a configured `max_cost_usd` is almost certainly a mistake
/// (a misplaced decimal point, a unit confusion) rather than an intended
/// budget, and is rejected outright rather than silently accepted.
const MAX_REPRESENTABLE_USD: f64 = 1_000_000_000.0;

#[derive(Debug, Clone, thiserror::Error)]
pub enum MicroUsdError {
    #[error("cost amount must be a finite number, got {0}")]
    NotFinite(f64),
    #[error("cost amount must not be negative, got {0}")]
    Negative(f64),
    #[error(
        "cost amount {0} exceeds the supported precision/range (max ${MAX_REPRESENTABLE_USD})"
    )]
    Unrepresentable(f64),
}

impl MicroUsd {
    pub const ZERO: MicroUsd = MicroUsd(0);

    /// Validates and converts a user-supplied dollar amount (e.g. a
    /// `max_cost_usd` config value). Rejects NaN, infinity, negative
    /// values, and anything too large to be a plausible budget.
    pub fn from_dollars_checked(value: f64) -> Result<Self, MicroUsdError> {
        if !value.is_finite() {
            return Err(MicroUsdError::NotFinite(value));
        }
        if value < 0.0 {
            return Err(MicroUsdError::Negative(value));
        }
        if value > MAX_REPRESENTABLE_USD {
            return Err(MicroUsdError::Unrepresentable(value));
        }
        let micros = (value * 1_000_000.0).round();
        if !micros.is_finite() || micros > i64::MAX as f64 {
            return Err(MicroUsdError::Unrepresentable(value));
        }
        Ok(MicroUsd(micros as i64))
    }

    /// Converts a provider-reported cost (always non-negative in practice)
    /// into the ledger's fixed-point representation. Clamps rather than
    /// rejects: a settled charge must always be recordable, even if the
    /// upstream ever reports a pathological value (negative due to a
    /// provider bug, or absurdly large) — silently dropping a real charge
    /// would understate spend, which is the wrong direction to fail in for
    /// a budget guard.
    pub fn from_settled_cost(value: f64) -> Self {
        Self::from_dollars_lossy(value)
    }

    /// Converts an already-validated `max_cost_usd` config snapshot (a
    /// `WorkflowRun`/`WorkflowDefinition` field that passed
    /// [`crate::workflow::entity::validate_max_cost_usd`] at create/update
    /// time) into the ledger's fixed-point representation. Infallible by
    /// construction — validation already happened — this is a defensive
    /// fallback for the conversion, not a second validation pass.
    pub fn from_validated_limit(value: f64) -> Self {
        Self::from_dollars_lossy(value)
    }

    /// Infallible dollars→micros conversion for a value already known to
    /// be safe to record (a provider-reported cost, or a pre-validated
    /// config limit). Non-finite or negative collapses to zero; too large
    /// saturates to `i64::MAX`. Never panics — a budget guard must always
    /// be able to record a number.
    fn from_dollars_lossy(value: f64) -> Self {
        if !value.is_finite() || value <= 0.0 {
            return MicroUsd::ZERO;
        }
        let micros = (value * 1_000_000.0).round();
        if micros >= i64::MAX as f64 {
            return MicroUsd(i64::MAX);
        }
        MicroUsd(micros as i64)
    }

    pub fn as_dollars(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    pub fn saturating_add(self, other: Self) -> Self {
        MicroUsd(self.0.saturating_add(other.0))
    }

    /// Clamped at zero — used for `overshoot_usd = max(0, spent - limit)`.
    pub fn saturating_sub(self, other: Self) -> Self {
        MicroUsd(self.0.saturating_sub(other.0).max(0))
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for MicroUsd {
    /// Two-decimal dollar rendering for human-facing diagnostics (the
    /// deterministic budget-stop message). Full precision is preserved in
    /// the underlying value and in `as_dollars()`'s JSON/API surface — this
    /// impl is for display text only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${:.2}", self.as_dollars())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_typical_dollar_values() {
        for v in [0.0, 5.0, 0.01, 4.80, 0.35, 5.15, 0.1, 1234.56] {
            let micro = MicroUsd::from_dollars_checked(v).unwrap();
            assert!(
                (micro.as_dollars() - v).abs() < 1e-9,
                "{v} round-tripped to {}",
                micro.as_dollars()
            );
        }
    }

    #[test]
    fn rejects_negative_nan_infinite_and_absurd_values() {
        assert!(MicroUsd::from_dollars_checked(-1.0).is_err());
        assert!(MicroUsd::from_dollars_checked(f64::NAN).is_err());
        assert!(MicroUsd::from_dollars_checked(f64::INFINITY).is_err());
        assert!(MicroUsd::from_dollars_checked(f64::NEG_INFINITY).is_err());
        assert!(MicroUsd::from_dollars_checked(MAX_REPRESENTABLE_USD * 10.0).is_err());
    }

    #[test]
    fn accepts_zero() {
        assert_eq!(MicroUsd::from_dollars_checked(0.0).unwrap(), MicroUsd::ZERO);
    }

    #[test]
    fn overshoot_example_from_handoff() {
        // $4.80 admitted, $0.35 settled -> $5.15 spent, $0.15 overshoot.
        let spent = MicroUsd::from_dollars_checked(4.80)
            .unwrap()
            .saturating_add(MicroUsd::from_dollars_checked(0.35).unwrap());
        let limit = MicroUsd::from_dollars_checked(5.00).unwrap();
        assert!((spent.as_dollars() - 5.15).abs() < 1e-9);
        assert!(spent >= limit);
        let overshoot = spent.saturating_sub(limit);
        assert!((overshoot.as_dollars() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn exact_boundary_has_zero_overshoot() {
        let spent = MicroUsd::from_dollars_checked(5.00).unwrap();
        let limit = MicroUsd::from_dollars_checked(5.00).unwrap();
        assert!(spent >= limit);
        assert_eq!(spent.saturating_sub(limit), MicroUsd::ZERO);
    }

    #[test]
    fn from_settled_cost_clamps_pathological_provider_values() {
        assert_eq!(MicroUsd::from_settled_cost(f64::NAN), MicroUsd::ZERO);
        assert_eq!(MicroUsd::from_settled_cost(-5.0), MicroUsd::ZERO);
        assert_eq!(MicroUsd::from_settled_cost(0.0), MicroUsd::ZERO);
    }
}
