//! Error types for implied volatility calculation.

use pricelevel::PriceLevelError;

/// Errors specific to IV calculation.
///
/// `#[non_exhaustive]`: new failure modes may be added in a minor release, so
/// exhaustive `match`es outside this crate need a wildcard arm.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum IVError {
    /// No valid price available: an empty book, or
    /// [`PriceSource::LastTrade`](super::PriceSource::LastTrade) on a
    /// two-sided book that has not traded yet (#294).
    #[error("no valid price available from order book")]
    NoPriceAvailable,

    /// Spread too wide for reliable calculation.
    #[error("spread too wide: {spread_bps:.1} bps exceeds threshold of {threshold_bps:.1} bps")]
    SpreadTooWide {
        /// Current spread in basis points.
        spread_bps: f64,
        /// Maximum allowed spread in basis points.
        threshold_bps: f64,
    },

    /// The book is crossed (best ask below best bid) or locked (best ask equal
    /// to best bid), so no meaningful mid price exists for IV calculation.
    #[error("book is crossed or locked: best bid {bid:.4} is not below best ask {ask:.4}")]
    CrossedBook {
        /// Best bid price (scaled to f64).
        bid: f64,
        /// Best ask price (scaled to f64).
        ask: f64,
    },

    /// Newton-Raphson solver did not converge within max iterations.
    #[error("solver did not converge after {iterations} iterations, last IV: {last_iv:.4}")]
    ConvergenceFailure {
        /// Number of iterations attempted.
        iterations: u32,
        /// Last IV estimate before giving up.
        last_iv: f64,
    },

    /// Invalid input parameters for IV calculation.
    #[error("invalid parameters: {message}")]
    InvalidParams {
        /// Description of the invalid parameter.
        message: String,
    },

    /// A solver or IV configuration field ([`SolverConfig`] /
    /// [`IVConfig`]) is out of its valid domain (non-finite, non-positive,
    /// or `min_iv > max_iv`).
    ///
    /// Returned by `SolverConfig::validate` / `IVConfig::validate`, which every
    /// solve entry point calls before touching the configuration.
    ///
    /// [`SolverConfig`]: super::SolverConfig
    /// [`IVConfig`]: super::IVConfig
    #[error("invalid configuration: {field}: {message}")]
    InvalidConfig {
        /// Name of the offending configuration field.
        field: &'static str,
        /// Description of the constraint that was violated, including the value.
        message: String,
    },

    /// A pricing computation produced a non-finite (NaN or infinite) result
    /// from finite, in-domain inputs (for example an overflowing discount
    /// factor with an extreme `risk_free_rate`).
    #[error("{operation} produced a non-finite result: {value}")]
    NonFiniteResult {
        /// The computation that produced the value (e.g. `"price"`, `"vega"`).
        operation: &'static str,
        /// The offending non-finite value.
        value: f64,
    },

    /// An integer aggregate used while extracting a price overflowed.
    #[error("arithmetic overflow in {operation}")]
    ArithmeticOverflow {
        /// The computation that overflowed.
        operation: &'static str,
    },

    /// Reading a price level from the book failed (for example the level's
    /// total quantity overflows `u64`).
    #[error("price level error: {0}")]
    PriceLevel(#[from] PriceLevelError),

    /// Price is below intrinsic value (indicates arbitrage opportunity).
    #[error("price {price:.4} is below intrinsic value {intrinsic:.4}")]
    PriceBelowIntrinsic {
        /// Market price observed.
        price: f64,
        /// Calculated intrinsic value.
        intrinsic: f64,
    },

    /// Time to expiry is too small for reliable calculation.
    #[error("time to expiry {time_to_expiry:.6} years is below minimum {min_time:.6} years")]
    TimeToExpiryTooSmall {
        /// Time to expiry in years.
        time_to_expiry: f64,
        /// Minimum required time in years.
        min_time: f64,
    },

    /// Volatility is outside reasonable bounds.
    #[error("volatility {volatility:.4} is outside bounds [{min_bound:.4}, {max_bound:.4}]")]
    VolatilityOutOfBounds {
        /// Calculated volatility.
        volatility: f64,
        /// Minimum bound.
        min_bound: f64,
        /// Maximum bound.
        max_bound: f64,
    },
}

impl IVError {
    /// Builds an [`IVError::InvalidConfig`].
    #[cold]
    #[must_use]
    pub(crate) fn invalid_config(field: &'static str, message: String) -> Self {
        IVError::InvalidConfig { field, message }
    }

    /// Builds an [`IVError::InvalidParams`].
    #[cold]
    #[must_use]
    pub(crate) fn invalid_params(message: String) -> Self {
        IVError::InvalidParams { message }
    }

    /// Builds an [`IVError::NonFiniteResult`].
    #[cold]
    #[must_use]
    pub(crate) fn non_finite(operation: &'static str, value: f64) -> Self {
        IVError::NonFiniteResult { operation, value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = IVError::NoPriceAvailable;
        assert_eq!(err.to_string(), "no valid price available from order book");

        let err = IVError::SpreadTooWide {
            spread_bps: 600.0,
            threshold_bps: 500.0,
        };
        assert!(err.to_string().contains("600.0 bps"));

        let err = IVError::CrossedBook {
            bid: 10.5,
            ask: 10.0,
        };
        assert!(err.to_string().contains("crossed or locked"));

        let err = IVError::ConvergenceFailure {
            iterations: 100,
            last_iv: 0.25,
        };
        assert!(err.to_string().contains("100 iterations"));

        let err = IVError::InvalidParams {
            message: "negative spot price".to_string(),
        };
        assert!(err.to_string().contains("negative spot price"));

        let err = IVError::PriceBelowIntrinsic {
            price: 5.0,
            intrinsic: 10.0,
        };
        assert!(err.to_string().contains("below intrinsic"));

        let err = IVError::TimeToExpiryTooSmall {
            time_to_expiry: 0.0001,
            min_time: 0.001,
        };
        assert!(err.to_string().contains("time to expiry"));

        let err = IVError::VolatilityOutOfBounds {
            volatility: 6.0,
            min_bound: 0.001,
            max_bound: 5.0,
        };
        assert!(err.to_string().contains("outside bounds"));

        let err = IVError::invalid_config("min_iv", "must be <= max_iv".to_string());
        assert_eq!(
            err.to_string(),
            "invalid configuration: min_iv: must be <= max_iv"
        );

        let err = IVError::non_finite("price", f64::INFINITY);
        assert_eq!(err.to_string(), "price produced a non-finite result: inf");

        let err = IVError::ArithmeticOverflow {
            operation: "weighted mid total quantity",
        };
        assert!(err.to_string().contains("overflow"));

        let err = IVError::from(PriceLevelError::InvalidOperation {
            message: "price level total quantity overflow".to_string(),
        });
        assert!(err.to_string().starts_with("price level error:"));
    }
}
