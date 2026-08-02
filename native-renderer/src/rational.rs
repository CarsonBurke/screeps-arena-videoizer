use std::cmp::Ordering;
use std::fmt;

use crate::{Error, Result};

/// Reduced, positive rational used for exact frame/tick timestamps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rational {
    numerator: u128,
    denominator: u128,
}

impl Rational {
    pub const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    pub fn new(numerator: u128, denominator: u128) -> Result<Self> {
        if denominator == 0 {
            return Err(Error::Invalid(
                "a rational denominator cannot be zero".to_owned(),
            ));
        }
        let divisor = greatest_common_divisor(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub fn parse_rate(value: &str, name: &str) -> Result<Self> {
        let mut parts = value.split('/');
        let numerator = parts
            .next()
            .ok_or_else(|| Error::Invalid(format!("{name} must be a positive rate")))?;
        let denominator = parts.next();
        if parts.next().is_some() {
            return Err(Error::Invalid(format!(
                "{name} must be a positive decimal or rational"
            )));
        }
        let numerator = Self::parse_decimal(numerator, name)?;
        let value = if let Some(denominator) = denominator {
            let denominator = Self::parse_decimal(denominator, name)?;
            numerator.checked_div(denominator)?
        } else {
            numerator
        };
        if value.numerator == 0 {
            return Err(Error::Invalid(format!("{name} must be positive")));
        }
        Ok(value)
    }

    fn parse_decimal(value: &str, name: &str) -> Result<Self> {
        if value.is_empty()
            || value.starts_with('-')
            || value.starts_with('+')
            || value.contains(['e', 'E'])
        {
            return Err(Error::Invalid(format!(
                "{name} must be a positive decimal or rational"
            )));
        }
        let mut parts = value.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|part| {
                part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(Error::Invalid(format!(
                "{name} must be a positive decimal or rational"
            )));
        }
        let fraction = fraction.unwrap_or_default();
        let denominator = checked_pow10(fraction.len())?;
        let whole = whole
            .parse::<u128>()
            .map_err(|_| Error::ArithmeticOverflow)?;
        let fractional = if fraction.is_empty() {
            0
        } else {
            fraction
                .parse::<u128>()
                .map_err(|_| Error::ArithmeticOverflow)?
        };
        let numerator = whole
            .checked_mul(denominator)
            .and_then(|value| value.checked_add(fractional))
            .ok_or(Error::ArithmeticOverflow)?;
        Self::new(numerator, denominator)
    }

    pub const fn numerator(self) -> u128 {
        self.numerator
    }

    pub const fn denominator(self) -> u128 {
        self.denominator
    }

    pub fn checked_add(self, other: Self) -> Result<Self> {
        let numerator = self
            .numerator
            .checked_mul(other.denominator)
            .and_then(|left| {
                other
                    .numerator
                    .checked_mul(self.denominator)
                    .and_then(|right| left.checked_add(right))
            })
            .ok_or(Error::ArithmeticOverflow)?;
        let denominator = self
            .denominator
            .checked_mul(other.denominator)
            .ok_or(Error::ArithmeticOverflow)?;
        Self::new(numerator, denominator)
    }

    pub fn checked_sub(self, other: Self) -> Result<Self> {
        let left = self
            .numerator
            .checked_mul(other.denominator)
            .ok_or(Error::ArithmeticOverflow)?;
        let right = other
            .numerator
            .checked_mul(self.denominator)
            .ok_or(Error::ArithmeticOverflow)?;
        let numerator = left.checked_sub(right).ok_or_else(|| {
            Error::Invalid("rational subtraction would become negative".to_owned())
        })?;
        let denominator = self
            .denominator
            .checked_mul(other.denominator)
            .ok_or(Error::ArithmeticOverflow)?;
        Self::new(numerator, denominator)
    }

    pub fn checked_mul(self, other: Self) -> Result<Self> {
        // Cross-reduce first so ordinary replay rates stay far from u128 limits.
        let left_divisor = greatest_common_divisor(self.numerator, other.denominator);
        let right_divisor = greatest_common_divisor(other.numerator, self.denominator);
        let numerator = (self.numerator / left_divisor)
            .checked_mul(other.numerator / right_divisor)
            .ok_or(Error::ArithmeticOverflow)?;
        let denominator = (self.denominator / right_divisor)
            .checked_mul(other.denominator / left_divisor)
            .ok_or(Error::ArithmeticOverflow)?;
        Self::new(numerator, denominator)
    }

    pub fn checked_div(self, other: Self) -> Result<Self> {
        if other.numerator == 0 {
            return Err(Error::Invalid("cannot divide by zero".to_owned()));
        }
        self.checked_mul(Self::new(other.denominator, other.numerator)?)
    }

    pub fn floor(self) -> u128 {
        self.numerator / self.denominator
    }

    pub fn ceil(self) -> Result<u128> {
        self.numerator
            .checked_add(self.denominator - 1)
            .map(|value| value / self.denominator)
            .ok_or(Error::ArithmeticOverflow)
    }

    /// Positive half-up rounding, matching `virtual-timeline.js`.
    pub fn round(self) -> Result<u128> {
        self.numerator
            .checked_mul(2)
            .and_then(|value| value.checked_add(self.denominator))
            .map(|value| value / (2 * self.denominator))
            .ok_or(Error::ArithmeticOverflow)
    }

    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Self) -> Ordering {
        // Values produced by a validated replay timeline fit this product. All
        // constructors performing arithmetic above remain checked.
        self.numerator
            .checked_mul(other.denominator)
            .zip(other.numerator.checked_mul(self.denominator))
            .map_or_else(
                || self.as_f64().total_cmp(&other.as_f64()),
                |(left, right)| left.cmp(&right),
            )
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denominator == 1 {
            write!(formatter, "{}", self.numerator)
        } else {
            write!(formatter, "{}/{}", self.numerator, self.denominator)
        }
    }
}

fn checked_pow10(exponent: usize) -> Result<u128> {
    (0..exponent).try_fold(1_u128, |value, _| {
        value.checked_mul(10).ok_or(Error::ArithmeticOverflow)
    })
}

fn greatest_common_divisor(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

#[cfg(test)]
mod tests {
    use super::Rational;

    #[test]
    fn parses_and_reduces_rates() {
        assert_eq!(
            Rational::parse_rate("3.75", "rate").unwrap().to_string(),
            "15/4"
        );
        assert_eq!(
            Rational::parse_rate("30000/1001", "rate")
                .unwrap()
                .to_string(),
            "30000/1001"
        );
    }

    #[test]
    fn rejects_noncanonical_rate_syntax() {
        for value in ["", "0", "-1", "1e3", "1.", ".5", "1/0", "1/2/3"] {
            assert!(Rational::parse_rate(value, "rate").is_err(), "{value}");
        }
    }

    #[test]
    fn half_up_rounding_and_integer_ops_match_virtual_timeline() {
        assert_eq!(Rational::new(1, 2).unwrap().round().unwrap(), 1);
        assert_eq!(Rational::new(1, 3).unwrap().round().unwrap(), 0);
        assert_eq!(Rational::new(2, 3).unwrap().round().unwrap(), 1);
        assert_eq!(Rational::new(1, 3).unwrap().ceil().unwrap(), 1);
        assert_eq!(Rational::new(5, 2).unwrap().floor(), 2);

        let left = Rational::new(3, 4).unwrap();
        let right = Rational::new(1, 2).unwrap();
        assert_eq!(left.checked_sub(right).unwrap().to_string(), "1/4");
        assert!(right.checked_sub(left).is_err());
        assert!(left.checked_div(Rational::ZERO).is_err());

        // 1/3 second in microseconds uses half-up rounding to 333333.
        let micros = Rational::new(1, 3)
            .unwrap()
            .checked_mul(Rational::new(1_000_000, 1).unwrap())
            .unwrap()
            .round()
            .unwrap();
        assert_eq!(micros, 333_333);
    }
}
