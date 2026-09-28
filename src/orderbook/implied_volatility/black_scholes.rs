//! Black-Scholes pricing model and Greeks calculation.
//!
//! This module provides a lightweight implementation of the Black-Scholes
//! option pricing model for use in implied volatility calculations.
//!
//! # Domain and errors
//!
//! The pricing functions ([`BlackScholes::price`], the Greeks, [`BlackScholes::d1`]
//! and [`BlackScholes::d2`]) validate their inputs and return
//! `Result<f64, IVError>`: they never hand a NaN or an infinity back to the
//! caller. Inputs must be finite, `spot > 0` and `strike > 0`. `time_to_expiry`
//! and `vol` must be `>= 0` for price and Greeks (zero selects the documented
//! degenerate limit) and `> 0` for `d1` / `d2`. A finite, in-domain input can
//! still overflow (for example an extreme `risk_free_rate` makes the discount
//! factor infinite); such a result is reported as
//! [`IVError::NonFiniteResult`].
//!
//! The pure special functions ([`BlackScholes::erf`],
//! [`BlackScholes::norm_cdf`], [`BlackScholes::norm_pdf`]) stay infallible:
//! they are total on every finite input (and on `±inf`) and return a finite
//! value in their range. Only a NaN input yields NaN.

use super::error::IVError;
use super::types::{IVParams, OptionType};
use std::f64::consts::PI;

/// Square root of 2, precomputed for efficiency.
const SQRT_2: f64 = std::f64::consts::SQRT_2;

/// Days per year used to convert annual theta to daily theta.
const DAYS_PER_YEAR: f64 = 365.0;

/// Returns `value` if it is finite, otherwise [`IVError::NonFiniteResult`].
#[inline]
fn ensure_finite(operation: &'static str, value: f64) -> Result<f64, IVError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(IVError::non_finite(operation, value))
    }
}

/// Rejects a non-finite named input with [`IVError::InvalidParams`].
#[inline]
fn require_finite(name: &'static str, value: f64) -> Result<(), IVError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(IVError::invalid_params(format!(
            "{name} must be finite, got {value}"
        )))
    }
}

/// Rejects a non-positive (or non-finite) named input.
#[inline]
fn require_positive(name: &'static str, value: f64) -> Result<(), IVError> {
    require_finite(name, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(IVError::invalid_params(format!(
            "{name} must be positive, got {value}"
        )))
    }
}

/// Rejects a negative (or non-finite) named input. `-0.0` is accepted as zero.
#[inline]
fn require_non_negative(name: &'static str, value: f64) -> Result<(), IVError> {
    require_finite(name, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(IVError::invalid_params(format!(
            "{name} must be non-negative, got {value}"
        )))
    }
}

/// Validates the shared domain of price and Greeks: every input finite,
/// `spot > 0`, `strike > 0`, `time_to_expiry >= 0`, `vol >= 0`.
fn validate_pricing_inputs(params: &IVParams, vol: f64) -> Result<(), IVError> {
    require_positive("spot", params.spot)?;
    require_positive("strike", params.strike)?;
    require_finite("risk_free_rate", params.risk_free_rate)?;
    require_non_negative("time_to_expiry", params.time_to_expiry)?;
    require_non_negative("volatility", vol)?;
    Ok(())
}

/// Unchecked d1; callers validate inputs and check the final output.
#[inline]
fn d1_raw(spot: f64, strike: f64, rate: f64, time: f64, vol: f64) -> f64 {
    let sqrt_time = time.sqrt();
    ((spot / strike).ln() + (rate + 0.5 * vol * vol) * time) / (vol * sqrt_time)
}

/// Unchecked d2; callers validate inputs and check the final output.
#[inline]
fn d2_raw(d1: f64, vol: f64, time: f64) -> f64 {
    d1 - vol * time.sqrt()
}

/// Black-Scholes pricing model implementation.
///
/// Provides methods for calculating option prices and Greeks
/// using the Black-Scholes-Merton formula.
pub struct BlackScholes;

impl BlackScholes {
    /// Approximation of the error function (erf).
    ///
    /// Uses Abramowitz and Stegun approximation (formula 7.1.26)
    /// with maximum error of 1.5×10⁻⁷.
    ///
    /// Total on every finite input and on `±inf` (returns `±1.0`); the result
    /// is always in `[-1.0, 1.0]`. A NaN input yields NaN.
    ///
    /// # Arguments
    /// - `x`: Input value
    ///
    /// # Returns
    /// Approximation of erf(x)
    #[must_use]
    pub fn erf(x: f64) -> f64 {
        // Constants for the approximation
        const A1: f64 = 0.254829592;
        const A2: f64 = -0.284496736;
        const A3: f64 = 1.421413741;
        const A4: f64 = -1.453152027;
        const A5: f64 = 1.061405429;
        const P: f64 = 0.3275911;

        let sign = if x < 0.0 { -1.0 } else { 1.0 };
        let x = x.abs();

        let t = 1.0 / (1.0 + P * x);
        let y = 1.0 - (((((A5 * t + A4) * t) + A3) * t + A2) * t + A1) * t * (-x * x).exp();

        sign * y
    }

    /// Standard normal cumulative distribution function (CDF).
    ///
    /// Calculates P(Z ≤ x) where Z is a standard normal random variable.
    ///
    /// Total on every finite input and on `±inf`; the result is always in
    /// `[0.0, 1.0]`. A NaN input yields NaN.
    ///
    /// # Arguments
    /// - `x`: Input value
    ///
    /// # Returns
    /// Probability that a standard normal variable is less than or equal to x
    #[must_use]
    pub fn norm_cdf(x: f64) -> f64 {
        0.5 * (1.0 + Self::erf(x / SQRT_2))
    }

    /// Standard normal probability density function (PDF).
    ///
    /// Calculates the density of the standard normal distribution at x.
    ///
    /// Total on every finite input and on `±inf` (returns `0.0`); the result
    /// is always in `[0.0, 1/√(2π)]`. A NaN input yields NaN.
    ///
    /// # Arguments
    /// - `x`: Input value
    ///
    /// # Returns
    /// Density value at x
    #[must_use]
    pub fn norm_pdf(x: f64) -> f64 {
        (-0.5 * x * x).exp() / (2.0 * PI).sqrt()
    }

    /// Calculates the d1 parameter of the Black-Scholes formula.
    ///
    /// d1 = [ln(S/K) + (r + σ²/2)T] / (σ√T)
    ///
    /// # Arguments
    /// - `spot`: Current underlying price (S)
    /// - `strike`: Option strike price (K)
    /// - `rate`: Risk-free interest rate (r)
    /// - `time`: Time to expiration in years (T)
    /// - `vol`: Volatility (σ)
    ///
    /// # Returns
    /// The d1 parameter value
    ///
    /// # Errors
    ///
    /// - [`IVError::InvalidParams`] if any input is non-finite, or `spot`,
    ///   `strike`, `time` or `vol` is not strictly positive (d1 divides by
    ///   `σ√T`).
    /// - [`IVError::NonFiniteResult`] if the result overflows (for example an
    ///   extreme `spot / strike` ratio).
    #[must_use = "the d1 value (or error) must be handled"]
    pub fn d1(spot: f64, strike: f64, rate: f64, time: f64, vol: f64) -> Result<f64, IVError> {
        require_positive("spot", spot)?;
        require_positive("strike", strike)?;
        require_finite("rate", rate)?;
        require_positive("time", time)?;
        require_positive("volatility", vol)?;
        ensure_finite("d1", d1_raw(spot, strike, rate, time, vol))
    }

    /// Calculates the d2 parameter of the Black-Scholes formula.
    ///
    /// d2 = d1 - σ√T
    ///
    /// # Arguments
    /// - `d1`: The d1 parameter
    /// - `vol`: Volatility (σ)
    /// - `time`: Time to expiration in years (T)
    ///
    /// # Returns
    /// The d2 parameter value
    ///
    /// # Errors
    ///
    /// - [`IVError::InvalidParams`] if `d1` is non-finite or `vol` / `time` is
    ///   non-finite or not strictly positive.
    /// - [`IVError::NonFiniteResult`] if the result overflows.
    #[must_use = "the d2 value (or error) must be handled"]
    pub fn d2(d1: f64, vol: f64, time: f64) -> Result<f64, IVError> {
        require_finite("d1", d1)?;
        require_positive("volatility", vol)?;
        require_positive("time", time)?;
        ensure_finite("d2", d2_raw(d1, vol, time))
    }

    /// Calculates the theoretical option price using Black-Scholes formula.
    ///
    /// For calls: C = S·N(d1) - K·e^(-rT)·N(d2)
    /// For puts:  P = K·e^(-rT)·N(-d2) - S·N(-d1)
    ///
    /// Degenerate limits: with `time_to_expiry == 0` the price is the intrinsic
    /// value; with `vol == 0` it is the discounted intrinsic value
    /// `max(0, S - K·e^(-rT))` (call) / `max(0, K·e^(-rT) - S)` (put).
    ///
    /// # Arguments
    /// - `params`: Option parameters (spot, strike, time, rate, type)
    /// - `vol`: Volatility (σ)
    ///
    /// # Returns
    /// Theoretical option price
    ///
    /// # Errors
    ///
    /// - [`IVError::InvalidParams`] if any input is non-finite, `spot` or
    ///   `strike` is not strictly positive, or `time_to_expiry` / `vol` is
    ///   negative.
    /// - [`IVError::NonFiniteResult`] if the computed price is not finite.
    #[must_use = "the option price (or error) must be handled"]
    pub fn price(params: &IVParams, vol: f64) -> Result<f64, IVError> {
        validate_pricing_inputs(params, vol)?;

        // Handle edge cases
        if params.time_to_expiry == 0.0 {
            return ensure_finite("price", params.intrinsic_value());
        }

        let discount = (-params.risk_free_rate * params.time_to_expiry).exp();

        if vol == 0.0 {
            // With zero volatility, option is worth discounted intrinsic value
            let value = match params.option_type {
                OptionType::Call => (params.spot - params.strike * discount).max(0.0),
                OptionType::Put => (params.strike * discount - params.spot).max(0.0),
            };
            return ensure_finite("price", value);
        }

        let d1 = d1_raw(
            params.spot,
            params.strike,
            params.risk_free_rate,
            params.time_to_expiry,
            vol,
        );
        let d2 = d2_raw(d1, vol, params.time_to_expiry);

        let value = match params.option_type {
            OptionType::Call => {
                params.spot * Self::norm_cdf(d1) - params.strike * discount * Self::norm_cdf(d2)
            }
            OptionType::Put => {
                params.strike * discount * Self::norm_cdf(-d2) - params.spot * Self::norm_cdf(-d1)
            }
        };
        ensure_finite("price", value)
    }

    /// Calculates vega (∂price/∂σ) - sensitivity to volatility.
    ///
    /// Vega = S · N'(d1) · √T
    ///
    /// Vega is always non-negative for both calls and puts. Returns `0.0` when
    /// `time_to_expiry == 0` or `vol == 0`.
    ///
    /// # Arguments
    /// - `params`: Option parameters
    /// - `vol`: Current volatility estimate
    ///
    /// # Returns
    /// Vega value (change in price per unit change in volatility)
    ///
    /// # Errors
    ///
    /// Same domain as [`BlackScholes::price`]: [`IVError::InvalidParams`] for
    /// out-of-domain inputs, [`IVError::NonFiniteResult`] if the result is not
    /// finite.
    #[must_use = "the vega value (or error) must be handled"]
    pub fn vega(params: &IVParams, vol: f64) -> Result<f64, IVError> {
        validate_pricing_inputs(params, vol)?;
        if params.time_to_expiry == 0.0 || vol == 0.0 {
            return Ok(0.0);
        }

        let d1 = d1_raw(
            params.spot,
            params.strike,
            params.risk_free_rate,
            params.time_to_expiry,
            vol,
        );
        ensure_finite(
            "vega",
            params.spot * Self::norm_pdf(d1) * params.time_to_expiry.sqrt(),
        )
    }

    /// Calculates delta (∂price/∂S) - sensitivity to underlying price.
    ///
    /// For calls: Δ = N(d1)
    /// For puts:  Δ = N(d1) - 1
    ///
    /// Degenerate limits: at `time_to_expiry == 0` delta is the step on spot
    /// vs strike; with `vol == 0` it is the step on spot vs the discounted
    /// strike `K·e^(-rT)`.
    ///
    /// # Arguments
    /// - `params`: Option parameters
    /// - `vol`: Volatility
    ///
    /// # Returns
    /// Delta value
    ///
    /// # Errors
    ///
    /// Same domain as [`BlackScholes::price`]: [`IVError::InvalidParams`] for
    /// out-of-domain inputs, [`IVError::NonFiniteResult`] if the result is not
    /// finite.
    #[must_use = "the delta value (or error) must be handled"]
    pub fn delta(params: &IVParams, vol: f64) -> Result<f64, IVError> {
        validate_pricing_inputs(params, vol)?;

        if params.time_to_expiry == 0.0 || vol == 0.0 {
            let threshold = if params.time_to_expiry == 0.0 {
                params.strike
            } else {
                params.strike * (-params.risk_free_rate * params.time_to_expiry).exp()
            };
            return Ok(match params.option_type {
                OptionType::Call => {
                    if params.spot > threshold {
                        1.0
                    } else {
                        0.0
                    }
                }
                OptionType::Put => {
                    if params.spot < threshold {
                        -1.0
                    } else {
                        0.0
                    }
                }
            });
        }

        let d1 = d1_raw(
            params.spot,
            params.strike,
            params.risk_free_rate,
            params.time_to_expiry,
            vol,
        );

        let value = match params.option_type {
            OptionType::Call => Self::norm_cdf(d1),
            OptionType::Put => Self::norm_cdf(d1) - 1.0,
        };
        ensure_finite("delta", value)
    }

    /// Calculates gamma (∂²price/∂S²) - rate of change of delta.
    ///
    /// Γ = N'(d1) / (S · σ · √T)
    ///
    /// Gamma is always non-negative for both calls and puts. Returns `0.0`
    /// when `time_to_expiry == 0` or `vol == 0`.
    ///
    /// # Arguments
    /// - `params`: Option parameters
    /// - `vol`: Volatility
    ///
    /// # Returns
    /// Gamma value
    ///
    /// # Errors
    ///
    /// Same domain as [`BlackScholes::price`]: [`IVError::InvalidParams`] for
    /// out-of-domain inputs, [`IVError::NonFiniteResult`] if the result is not
    /// finite (for example a subnormal `spot · σ · √T` denominator).
    #[must_use = "the gamma value (or error) must be handled"]
    pub fn gamma(params: &IVParams, vol: f64) -> Result<f64, IVError> {
        validate_pricing_inputs(params, vol)?;
        if params.time_to_expiry == 0.0 || vol == 0.0 {
            return Ok(0.0);
        }

        let d1 = d1_raw(
            params.spot,
            params.strike,
            params.risk_free_rate,
            params.time_to_expiry,
            vol,
        );
        ensure_finite(
            "gamma",
            Self::norm_pdf(d1) / (params.spot * vol * params.time_to_expiry.sqrt()),
        )
    }

    /// Calculates theta (∂price/∂T) - time decay.
    ///
    /// Returns the daily theta (price change per day, annual theta / 365).
    /// Returns `0.0` when `time_to_expiry == 0`. With `vol == 0` it returns the
    /// deterministic carry term of the discounted intrinsic value.
    ///
    /// # Arguments
    /// - `params`: Option parameters
    /// - `vol`: Volatility
    ///
    /// # Returns
    /// Theta value (negative for long positions, representing time decay)
    ///
    /// # Errors
    ///
    /// Same domain as [`BlackScholes::price`]: [`IVError::InvalidParams`] for
    /// out-of-domain inputs, [`IVError::NonFiniteResult`] if the result is not
    /// finite.
    #[must_use = "the theta value (or error) must be handled"]
    pub fn theta(params: &IVParams, vol: f64) -> Result<f64, IVError> {
        validate_pricing_inputs(params, vol)?;
        if params.time_to_expiry == 0.0 {
            return Ok(0.0);
        }

        let discount = (-params.risk_free_rate * params.time_to_expiry).exp();
        let carry = params.risk_free_rate * params.strike * discount;

        let theta_annual = if vol == 0.0 {
            // σ → 0: the N'(d1)·σ term vanishes and N(±d2) becomes the step on
            // spot vs the discounted strike.
            let forward_strike = params.strike * discount;
            match params.option_type {
                OptionType::Call if params.spot > forward_strike => -carry,
                OptionType::Put if params.spot < forward_strike => carry,
                _ => 0.0,
            }
        } else {
            let d1 = d1_raw(
                params.spot,
                params.strike,
                params.risk_free_rate,
                params.time_to_expiry,
                vol,
            );
            let d2 = d2_raw(d1, vol, params.time_to_expiry);
            let sqrt_time = params.time_to_expiry.sqrt();

            let term1 = -params.spot * Self::norm_pdf(d1) * vol / (2.0 * sqrt_time);

            match params.option_type {
                OptionType::Call => term1 - carry * Self::norm_cdf(d2),
                OptionType::Put => term1 + carry * Self::norm_cdf(-d2),
            }
        };

        // Convert to daily theta
        ensure_finite("theta", theta_annual / DAYS_PER_YEAR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: f64 = 1e-6;

    #[test]
    fn test_erf() {
        // Test known values
        assert!((BlackScholes::erf(0.0) - 0.0).abs() < TOLERANCE);
        assert!((BlackScholes::erf(1.0) - 0.8427007929).abs() < 1e-5);
        assert!((BlackScholes::erf(-1.0) + 0.8427007929).abs() < 1e-5);
    }

    #[test]
    fn test_norm_cdf() {
        // N(0) = 0.5
        assert!((BlackScholes::norm_cdf(0.0) - 0.5).abs() < TOLERANCE);
        // N(-∞) ≈ 0, N(+∞) ≈ 1
        assert!(BlackScholes::norm_cdf(-10.0) < 1e-10);
        assert!(BlackScholes::norm_cdf(10.0) > 1.0 - 1e-10);
    }

    #[test]
    fn test_norm_pdf() {
        // PDF at 0 = 1/√(2π) ≈ 0.3989
        assert!((BlackScholes::norm_pdf(0.0) - 0.3989422804).abs() < TOLERANCE);
        // PDF is symmetric
        assert!((BlackScholes::norm_pdf(1.0) - BlackScholes::norm_pdf(-1.0)).abs() < TOLERANCE);
    }

    #[test]
    fn test_call_price_atm() {
        // ATM call with 25% vol, 1 year, no rates
        let params = IVParams::call(100.0, 100.0, 1.0, 0.0);
        let price = BlackScholes::price(&params, 0.25).unwrap();
        // ATM call ≈ 0.4 * S * σ * √T for small σ
        assert!(price > 9.0 && price < 11.0);
    }

    #[test]
    fn test_put_price_atm() {
        // ATM put with 25% vol, 1 year, no rates
        let params = IVParams::put(100.0, 100.0, 1.0, 0.0);
        let price = BlackScholes::price(&params, 0.25).unwrap();
        // Put-call parity: C - P = S - K*e^(-rT) = 0 when r=0 and S=K
        let call_params = IVParams::call(100.0, 100.0, 1.0, 0.0);
        let call_price = BlackScholes::price(&call_params, 0.25).unwrap();
        assert!((price - call_price).abs() < TOLERANCE);
    }

    #[test]
    fn test_put_call_parity() {
        // C - P = S - K*e^(-rT)
        let spot = 100.0;
        let strike = 105.0;
        let time = 0.5;
        let rate = 0.05;
        let vol = 0.3;

        let call_params = IVParams::call(spot, strike, time, rate);
        let put_params = IVParams::put(spot, strike, time, rate);

        let call_price = BlackScholes::price(&call_params, vol).unwrap();
        let put_price = BlackScholes::price(&put_params, vol).unwrap();

        let expected_diff = spot - strike * (-rate * time).exp();
        assert!((call_price - put_price - expected_diff).abs() < TOLERANCE);
    }

    #[test]
    fn test_vega_positive() {
        let params = IVParams::call(100.0, 100.0, 0.25, 0.05);
        let vega = BlackScholes::vega(&params, 0.25).unwrap();
        assert!(vega > 0.0);

        let put_params = IVParams::put(100.0, 100.0, 0.25, 0.05);
        let put_vega = BlackScholes::vega(&put_params, 0.25).unwrap();
        assert!(put_vega > 0.0);

        // Vega should be same for call and put
        assert!((vega - put_vega).abs() < TOLERANCE);
    }

    #[test]
    fn test_delta_bounds() {
        let call_params = IVParams::call(100.0, 100.0, 0.25, 0.05);
        let call_delta = BlackScholes::delta(&call_params, 0.25).unwrap();
        // Call delta should be between 0 and 1
        assert!(call_delta > 0.0 && call_delta < 1.0);

        let put_params = IVParams::put(100.0, 100.0, 0.25, 0.05);
        let put_delta = BlackScholes::delta(&put_params, 0.25).unwrap();
        // Put delta should be between -1 and 0
        assert!(put_delta > -1.0 && put_delta < 0.0);

        // Delta relationship: call_delta - put_delta = 1
        assert!((call_delta - put_delta - 1.0).abs() < TOLERANCE);
    }

    #[test]
    fn test_gamma_positive() {
        let params = IVParams::call(100.0, 100.0, 0.25, 0.05);
        let gamma = BlackScholes::gamma(&params, 0.25).unwrap();
        assert!(gamma > 0.0);
    }

    #[test]
    fn test_theta_negative_for_long() {
        let params = IVParams::call(100.0, 100.0, 0.25, 0.0);
        let theta = BlackScholes::theta(&params, 0.25).unwrap();
        // Theta is typically negative (time decay)
        assert!(theta < 0.0);
    }

    #[test]
    fn test_price_at_expiry() {
        // At expiry, option is worth intrinsic value
        let itm_call = IVParams::call(110.0, 100.0, 0.0, 0.05);
        let price = BlackScholes::price(&itm_call, 0.25).unwrap();
        assert!((price - 10.0).abs() < TOLERANCE);

        let otm_call = IVParams::call(90.0, 100.0, 0.0, 0.05);
        let price = BlackScholes::price(&otm_call, 0.25).unwrap();
        assert!(price.abs() < TOLERANCE);
    }

    #[test]
    fn test_deep_itm_call() {
        // Deep ITM call should be close to intrinsic
        let params = IVParams::call(150.0, 100.0, 0.25, 0.0);
        let price = BlackScholes::price(&params, 0.25).unwrap();
        assert!(price > 50.0);
    }

    #[test]
    fn test_deep_otm_call() {
        // Deep OTM call should be close to 0
        let params = IVParams::call(50.0, 100.0, 0.25, 0.0);
        let price = BlackScholes::price(&params, 0.25).unwrap();
        assert!(price < 0.01);
    }

    /// Edge floats exercised by the no-panic / no-NaN property loops.
    const EDGE_FLOATS: [f64; 12] = [
        0.0,
        -0.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
        5e-324, // smallest subnormal
        f64::MAX,
        f64::MIN,
        -1.0,
        1.0,
        1e-300,
    ];

    type Greek = fn(&IVParams, f64) -> Result<f64, IVError>;

    const GREEKS: [(&str, Greek); 5] = [
        ("price", BlackScholes::price),
        ("vega", BlackScholes::vega),
        ("delta", BlackScholes::delta),
        ("gamma", BlackScholes::gamma),
        ("theta", BlackScholes::theta),
    ];

    #[test]
    fn test_greeks_reject_non_finite_and_out_of_domain_inputs() {
        let good = IVParams::call(100.0, 100.0, 0.25, 0.05);
        let bad_params = [
            IVParams::call(f64::NAN, 100.0, 0.25, 0.05),
            IVParams::call(f64::INFINITY, 100.0, 0.25, 0.05),
            IVParams::call(0.0, 100.0, 0.25, 0.05),
            IVParams::call(-1.0, 100.0, 0.25, 0.05),
            IVParams::call(100.0, f64::NAN, 0.25, 0.05),
            IVParams::call(100.0, 0.0, 0.25, 0.05),
            IVParams::call(100.0, -5.0, 0.25, 0.05),
            IVParams::call(100.0, 100.0, f64::NAN, 0.05),
            IVParams::call(100.0, 100.0, f64::INFINITY, 0.05),
            IVParams::call(100.0, 100.0, -0.25, 0.05),
            IVParams::call(100.0, 100.0, 0.25, f64::NAN),
            IVParams::call(100.0, 100.0, 0.25, f64::NEG_INFINITY),
        ];
        for (name, greek) in GREEKS {
            for params in &bad_params {
                let result = greek(params, 0.25);
                assert!(
                    matches!(result, Err(IVError::InvalidParams { .. })),
                    "{name}({params:?}) must be InvalidParams, got {result:?}"
                );
            }
            for vol in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1] {
                let result = greek(&good, vol);
                assert!(
                    matches!(result, Err(IVError::InvalidParams { .. })),
                    "{name}(vol={vol}) must be InvalidParams, got {result:?}"
                );
            }
        }
    }

    #[test]
    fn test_greeks_zero_vol_and_zero_time_limits() {
        let call = IVParams::call(110.0, 100.0, 0.25, 0.05);
        let put = IVParams::put(90.0, 100.0, 0.25, 0.05);
        let discount = (-0.05_f64 * 0.25).exp();

        // vol == 0: discounted intrinsic, step delta, zero vega/gamma.
        let price = BlackScholes::price(&call, 0.0).unwrap();
        assert!((price - (110.0 - 100.0 * discount)).abs() < TOLERANCE);
        assert!((BlackScholes::delta(&call, 0.0).unwrap() - 1.0).abs() < TOLERANCE);
        assert!((BlackScholes::delta(&put, 0.0).unwrap() + 1.0).abs() < TOLERANCE);
        assert_eq!(BlackScholes::vega(&call, 0.0).unwrap(), 0.0);
        assert_eq!(BlackScholes::gamma(&call, 0.0).unwrap(), 0.0);
        // Deep ITM call with vol == 0 decays by the carry term.
        let theta = BlackScholes::theta(&call, 0.0).unwrap();
        assert!((theta - (-0.05 * 100.0 * discount / 365.0)).abs() < TOLERANCE);
        // -0.0 is treated as zero, not as negative.
        assert!(BlackScholes::price(&call, -0.0).is_ok());

        // time == 0: intrinsic, step delta, zero vega/gamma/theta.
        let expired = IVParams::call(110.0, 100.0, 0.0, 0.05);
        assert!((BlackScholes::price(&expired, 0.25).unwrap() - 10.0).abs() < TOLERANCE);
        assert!((BlackScholes::delta(&expired, 0.25).unwrap() - 1.0).abs() < TOLERANCE);
        assert_eq!(BlackScholes::vega(&expired, 0.25).unwrap(), 0.0);
        assert_eq!(BlackScholes::gamma(&expired, 0.25).unwrap(), 0.0);
        assert_eq!(BlackScholes::theta(&expired, 0.25).unwrap(), 0.0);
    }

    #[test]
    fn test_d1_d2_validation() {
        let d1 = BlackScholes::d1(100.0, 100.0, 0.05, 0.25, 0.25).unwrap();
        let d2 = BlackScholes::d2(d1, 0.25, 0.25).unwrap();
        assert!((d1 - d2 - 0.25 * 0.5).abs() < TOLERANCE);

        // d1 divides by σ√T: zero vol / time are rejected, not inf.
        for (spot, strike, rate, time, vol) in [
            (100.0, 100.0, 0.05, 0.0, 0.25),
            (100.0, 100.0, 0.05, 0.25, 0.0),
            (0.0, 100.0, 0.05, 0.25, 0.25),
            (100.0, 0.0, 0.05, 0.25, 0.25),
            (100.0, 100.0, f64::NAN, 0.25, 0.25),
        ] {
            assert!(matches!(
                BlackScholes::d1(spot, strike, rate, time, vol),
                Err(IVError::InvalidParams { .. })
            ));
        }
        assert!(matches!(
            BlackScholes::d2(f64::NAN, 0.25, 0.25),
            Err(IVError::InvalidParams { .. })
        ));
        assert!(matches!(
            BlackScholes::d2(0.1, 0.0, 0.25),
            Err(IVError::InvalidParams { .. })
        ));
        // Finite, in-domain inputs whose ratio overflows report NonFiniteResult.
        assert!(matches!(
            BlackScholes::d1(f64::MAX, f64::MIN_POSITIVE, 0.0, 1.0, 0.25),
            Err(IVError::NonFiniteResult {
                operation: "d1",
                ..
            })
        ));
    }

    #[test]
    fn test_price_non_finite_result_is_typed() {
        // A huge negative rate makes the discount factor overflow to +inf.
        let params = IVParams::put(100.0, 100.0, 1.0, -1e300);
        let result = BlackScholes::price(&params, 0.25);
        assert!(
            matches!(result, Err(IVError::NonFiniteResult { .. })),
            "got {result:?}"
        );
    }

    #[test]
    fn test_edge_floats_never_panic_or_return_non_finite() {
        for &x in &EDGE_FLOATS {
            // Special functions: NaN in, NaN out; otherwise finite and in range.
            for value in [
                BlackScholes::erf(x),
                BlackScholes::norm_cdf(x),
                BlackScholes::norm_pdf(x),
            ] {
                assert!(value.is_finite() || x.is_nan(), "x={x} -> {value}");
            }
            for &y in &EDGE_FLOATS {
                if let Ok(v) = BlackScholes::d2(x, y, y) {
                    assert!(v.is_finite());
                }
                if let Ok(v) = BlackScholes::d1(x, y, x, y, x) {
                    assert!(v.is_finite());
                }
                for option_type in [OptionType::Call, OptionType::Put] {
                    let params = IVParams::new(x, y, x.abs(), y, option_type);
                    for (name, greek) in GREEKS {
                        for &vol in &EDGE_FLOATS {
                            if let Ok(v) = greek(&params, vol) {
                                assert!(v.is_finite(), "{name}({params:?}, {vol}) = {v}");
                            }
                        }
                    }
                }
            }
        }
    }
}
