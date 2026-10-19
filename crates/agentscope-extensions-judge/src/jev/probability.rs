//! The probability-sum acceptance rule for System One answers.
//!
//! Java commit `a5be76105` (*fix(jev): accept probability sums within the API's 4-decimal
//! rounding bound*) relaxed `PROBABILITY_SUM_TOLERANCE_PER_OPTION` from `1e-6` to `5e-5`:
//!
//! > The System One API returns probabilities rounded to 4 decimal places, so each value carries
//! > up to 5e-5 of rounding error. The sum of n values may therefore drift from 1 by up to
//! > n * 5e-5 without any individual probability being wrong.
//!
//! [`probability_sum_tolerance`] therefore scales with the option count, exactly like
//! `JevClient.validateProbabilities` (`Math.abs(sum - 1) > PROBABILITY_SUM_TOLERANCE_PER_OPTION *
//! probabilities.size()`).

use std::collections::BTreeMap;

/// Half of the smallest decimal place the API reports (`0.0001 / 2`).
///
/// Equals Java's `JevClient.PROBABILITY_SUM_TOLERANCE_PER_OPTION` after `a5be76105`.
pub const PROBABILITY_SUM_TOLERANCE_PER_OPTION: f64 = 0.00005;

/// Decimal places the System One API rounds probabilities to.
pub const API_PROBABILITY_DECIMAL_PLACES: u32 = 4;

/// The per-value rounding bound implied by [`API_PROBABILITY_DECIMAL_PLACES`].
pub const API_PROBABILITY_ROUNDING_BOUND: f64 = 0.00005;

/// The tolerance allowed for a sum of `option_count` probabilities.
///
/// Zero options yield a zero tolerance (an empty distribution can never sum to 1).
pub fn probability_sum_tolerance(option_count: usize) -> f64 {
    PROBABILITY_SUM_TOLERANCE_PER_OPTION * option_count as f64
}

/// Whether `sum` is within the rounding bound of `1` for `option_count` probabilities.
///
/// This is the exact predicate Java uses (`>` rejects, `==` accepts).
#[must_use]
pub fn sum_within_tolerance(sum: f64, option_count: usize) -> bool {
    (sum - 1.0).abs() <= probability_sum_tolerance(option_count)
}

/// Sums a probability distribution, preserving iteration order of `values`.
#[must_use]
pub fn sum_probabilities(values: impl IntoIterator<Item = f64>) -> f64 {
    values.into_iter().sum()
}

/// Sums a keyed probability distribution.
#[must_use]
pub fn sum_keyed(probabilities: &BTreeMap<String, f64>) -> f64 {
    sum_probabilities(probabilities.values().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 34-option distribution captured in Java commit `a5be76105`
    /// (`JevClientTest.acceptsProbabilitiesRoundedToApiPrecision`). Its 4-decimal probabilities
    /// sum to 1.0001, i.e. 1e-4 of drift, which the previous 1e-6-per-option bound rejected.
    const CAPTURED: [f64; 34] = [
        0.0064, 0.0165, 0.0061, 0.0232, 0.0411, 0.007, 0.0357, 0.0227, 0.022, 0.0074, 0.0187,
        0.0224, 0.0051, 0.0081, 0.0105, 0.0402, 0.0076, 0.0081, 0.02, 0.256, 0.0152, 0.1152,
        0.0562, 0.0145, 0.0038, 0.0036, 0.0232, 0.0427, 0.0079, 0.0116, 0.01, 0.0072, 0.0223,
        0.0819,
    ];

    #[test]
    fn tolerance_is_half_of_the_reported_decimal_place() {
        assert_eq!(
            API_PROBABILITY_ROUNDING_BOUND,
            0.5 * 10f64.powi(-(API_PROBABILITY_DECIMAL_PLACES as i32))
        );
        assert_eq!(
            PROBABILITY_SUM_TOLERANCE_PER_OPTION,
            API_PROBABILITY_ROUNDING_BOUND
        );
    }

    #[test]
    fn tolerance_scales_with_option_count() {
        assert_eq!(probability_sum_tolerance(0), 0.0);
        assert_eq!(probability_sum_tolerance(1), 0.00005);
        assert_eq!(probability_sum_tolerance(34), 0.00005 * 34.0);
        assert!((probability_sum_tolerance(34) - 0.0017).abs() < 1e-12);
        assert_eq!(probability_sum_tolerance(255), 0.00005 * 255.0);
        assert!((probability_sum_tolerance(255) - 0.01275).abs() < 1e-12);
    }

    #[test]
    fn accepts_captured_34_option_distribution() {
        let sum = sum_probabilities(CAPTURED);
        assert!((sum - 1.0001).abs() < 1e-12, "unexpected sum {sum}");
        assert!(sum_within_tolerance(sum, CAPTURED.len()));
    }

    #[test]
    fn captured_distribution_would_have_failed_the_old_tolerance() {
        // The regression this module documents: 1e-6 per option rejected a real response.
        const OLD_TOLERANCE_PER_OPTION: f64 = 0.000001;
        let sum = sum_probabilities(CAPTURED);
        let old_tolerance = OLD_TOLERANCE_PER_OPTION * CAPTURED.len() as f64;
        assert!(
            (sum - 1.0).abs() > old_tolerance,
            "the fixture no longer demonstrates the 1e-6 regression"
        );
    }

    #[test]
    fn accepts_sums_rounded_within_the_bound() {
        // 10 x 0.1 + 0.0001 -> 1.0001, well inside 10 * 5e-5 = 5e-4.
        let mut high = vec![0.1f64; 9];
        high.push(0.1001);
        let sum_high = sum_probabilities(high);
        assert!(sum_within_tolerance(sum_high, 10));

        // 10 x 0.1 - 0.0001 -> 0.9999.
        let mut low = vec![0.1f64; 9];
        low.push(0.0999);
        let sum_low = sum_probabilities(low);
        assert!(sum_within_tolerance(sum_low, 10));

        // Two options: the bound is exactly 1e-4, so 0.9999 and 1.0001 both pass.
        assert!(sum_within_tolerance(0.9999, 2));
        assert!(sum_within_tolerance(1.0001, 2));
    }

    #[test]
    fn rejects_genuine_drift() {
        // 0.5 + 0.49: not a rounding artefact.
        assert!(!sum_within_tolerance(0.99, 2));
        // Java's reject case: 34 options where option_0 is inflated by 0.01, so the sum becomes
        // ~1.01, far beyond 34 * 5e-5 = 1.7e-3.
        let mut drifted: Vec<f64> = vec![1.0 / 34.0; 34];
        drifted[0] = 1.0 / 34.0 + 0.01;
        let sum = sum_probabilities(drifted);
        assert!((sum - 1.01).abs() < 1e-9, "unexpected sum {sum}");
        assert!((sum - 1.0).abs() > probability_sum_tolerance(34));
        assert!(!sum_within_tolerance(sum, 34));
    }

    #[test]
    fn accepts_the_255_option_scaling_case() {
        // Java `scalesProbabilitySumToleranceWithOptionCount`: 255 options, one inflated by 1e-5.
        let mut probabilities: BTreeMap<String, f64> = BTreeMap::new();
        for index in 0..255 {
            let mut probability = 1.0 / 255.0;
            if index == 0 {
                probability += 0.00001;
            }
            probabilities.insert(format!("option_{index}"), probability);
        }
        let sum = sum_keyed(&probabilities);
        assert!(sum_within_tolerance(sum, probabilities.len()));
        // The bound is per-option, so a zero-option distribution is never within tolerance
        // (its bound is zero) — and a drift the 255-option bound forgives is caught when the
        // same distribution is (incorrectly) scored against fewer options only if the drift
        // exceeds that smaller bound. 1e-5 does not, so assert the scaling property directly.
        assert!(!sum_within_tolerance(sum, 0));
        assert!(!sum_within_tolerance(1.0 + 0.0002, 2));
        assert!(probability_sum_tolerance(255) > probability_sum_tolerance(2));
    }

    #[test]
    fn empty_distribution_never_sums_to_one() {
        assert!(!sum_within_tolerance(
            sum_probabilities(std::iter::empty()),
            0
        ));
        assert_eq!(sum_keyed(&BTreeMap::new()), 0.0);
    }
}
