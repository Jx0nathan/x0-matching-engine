use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// Price in the symbol's minor quote units. Scale and tick-alignment policy
/// live in `SymbolSpec`, never on this type.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Price(pub i64);

/// Quantity in the symbol's minor base units. Lot-alignment lives in `SymbolSpec`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Size(pub i64);

/// Monetary amount in some currency's minor units. Signed: PnL can be negative.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Amount(pub i64);

/// Basis points: 1 bp = 0.01% = 1/10_000. Stored as u32 (10_000 bps = 100%).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Bps(pub u32);

impl Price {
    pub const ZERO: Self = Self(0);
    pub const MIN: Self = Self(i64::MIN);
    pub const MAX: Self = Self(i64::MAX);

    #[inline]
    pub const fn new(v: i64) -> Self {
        Self(v)
    }
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// Raw product `Price × Size` widened to i128. Always safe — i64×i64 fits
    /// in i128 with room to spare. Caller applies symbol scale and rounding.
    #[inline]
    pub fn mul_size(self, size: Size) -> i128 {
        (self.0 as i128) * (size.0 as i128)
    }
}

impl Size {
    pub const ZERO: Self = Self(0);
    pub const MIN: Self = Self(i64::MIN);
    pub const MAX: Self = Self(i64::MAX);

    #[inline]
    pub const fn new(v: i64) -> Self {
        Self(v)
    }
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
    #[inline]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    #[inline]
    pub fn checked_add(self, rhs: Size) -> Option<Size> {
        self.0.checked_add(rhs.0).map(Size)
    }

    #[inline]
    pub fn checked_sub(self, rhs: Size) -> Option<Size> {
        self.0.checked_sub(rhs.0).map(Size)
    }

    /// i64-saturating subtraction. Does NOT clamp at zero — caller's job if needed.
    #[inline]
    pub fn saturating_sub(self, rhs: Size) -> Size {
        Size(self.0.saturating_sub(rhs.0))
    }

    #[inline]
    pub fn min(self, other: Size) -> Size {
        Size(self.0.min(other.0))
    }

    #[inline]
    pub fn max(self, other: Size) -> Size {
        Size(self.0.max(other.0))
    }
}

impl Amount {
    pub const ZERO: Self = Self(0);
    pub const MIN: Self = Self(i64::MIN);
    pub const MAX: Self = Self(i64::MAX);

    #[inline]
    pub const fn new(v: i64) -> Self {
        Self(v)
    }
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn checked_add(self, rhs: Amount) -> Option<Amount> {
        self.0.checked_add(rhs.0).map(Amount)
    }

    #[inline]
    pub fn checked_sub(self, rhs: Amount) -> Option<Amount> {
        self.0.checked_sub(rhs.0).map(Amount)
    }

    /// Apply a bps rate, truncating toward zero.
    /// For fees that should round in favor of the exchange, use `mul_bps_ceil`.
    #[inline]
    pub fn mul_bps(self, bps: Bps) -> Option<Amount> {
        let product = (self.0 as i128).checked_mul(bps.0 as i128)?;
        i64::try_from(product / 10_000).ok().map(Amount)
    }

    /// Apply a bps rate, rounding **away** from zero. Use for fee collection
    /// so we never undercharge a sub-cent fraction.
    #[inline]
    pub fn mul_bps_ceil(self, bps: Bps) -> Option<Amount> {
        let product = (self.0 as i128).checked_mul(bps.0 as i128)?;
        let result = if product >= 0 {
            (product + 9_999) / 10_000
        } else {
            (product - 9_999) / 10_000
        };
        i64::try_from(result).ok().map(Amount)
    }

    /// Truncate an i128 (typically `Price::mul_size` output) by a divisor and
    /// fit into i64. Returns None on i64 overflow.
    #[inline]
    pub fn from_scaled_i128(raw: i128, divisor: i128) -> Option<Amount> {
        if divisor == 0 {
            return None;
        }
        i64::try_from(raw / divisor).ok().map(Amount)
    }
}

impl Bps {
    pub const ZERO: Self = Self(0);
    pub const ONE_PERCENT: Self = Self(100);
    pub const FULL: Self = Self(10_000);

    #[inline]
    pub const fn new(v: u32) -> Self {
        Self(v)
    }
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl fmt::Display for Bps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}bps", self.0)
    }
}

// Operator overloads. Same overflow semantics as primitive i64:
// panic in debug builds, wrap in release. For untrusted input on hot paths,
// use the explicit `checked_*` methods instead.
impl Add for Size {
    type Output = Size;
    #[inline]
    fn add(self, rhs: Self) -> Size {
        Size(self.0 + rhs.0)
    }
}
impl Sub for Size {
    type Output = Size;
    #[inline]
    fn sub(self, rhs: Self) -> Size {
        Size(self.0 - rhs.0)
    }
}
impl AddAssign for Size {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}
impl SubAssign for Size {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl Add for Amount {
    type Output = Amount;
    #[inline]
    fn add(self, rhs: Self) -> Amount {
        Amount(self.0 + rhs.0)
    }
}
impl Sub for Amount {
    type Output = Amount;
    #[inline]
    fn sub(self, rhs: Self) -> Amount {
        Amount(self.0 - rhs.0)
    }
}
impl AddAssign for Amount {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}
impl SubAssign for Amount {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}
impl Neg for Amount {
    type Output = Amount;
    #[inline]
    fn neg(self) -> Amount {
        Amount(-self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn price_size_product_fits_in_i128() {
        let product = Price(i64::MAX).mul_size(Size(i64::MAX));
        assert!(product > 0);
        assert!(product < i128::MAX);
    }

    #[test]
    fn checked_add_detects_overflow() {
        assert_eq!(Amount(i64::MAX).checked_add(Amount(1)), None);
    }

    #[test]
    fn checked_sub_detects_underflow() {
        assert_eq!(Amount(i64::MIN).checked_sub(Amount(1)), None);
    }

    #[test]
    fn mul_bps_truncates_toward_zero() {
        assert_eq!(Amount(10_001).mul_bps(Bps(1)), Some(Amount(1)));
        assert_eq!(Amount(-10_001).mul_bps(Bps(1)), Some(Amount(-1)));
    }

    #[test]
    fn mul_bps_ceil_rounds_away_from_zero() {
        assert_eq!(Amount(10_001).mul_bps_ceil(Bps(1)), Some(Amount(2)));
        assert_eq!(Amount(-10_001).mul_bps_ceil(Bps(1)), Some(Amount(-2)));
    }

    #[test]
    fn mul_bps_zero_amount_is_zero() {
        assert_eq!(Amount(0).mul_bps(Bps(500)), Some(Amount(0)));
        assert_eq!(Amount(0).mul_bps_ceil(Bps(500)), Some(Amount(0)));
    }

    #[test]
    fn from_scaled_i128_zero_divisor_returns_none() {
        assert_eq!(Amount::from_scaled_i128(100, 0), None);
    }

    #[test]
    fn from_scaled_i128_overflow_returns_none() {
        assert_eq!(Amount::from_scaled_i128(i128::MAX, 1), None);
    }

    proptest! {
        #[test]
        fn add_then_sub_roundtrip(a: i64, b: i64) {
            let av = Amount(a);
            let bv = Amount(b);
            if let Some(sum) = av.checked_add(bv) {
                prop_assert_eq!(sum.checked_sub(bv), Some(av));
            }
        }

        #[test]
        fn mul_bps_bounded_by_amount(a in -1_000_000_000_000i64..1_000_000_000_000i64, bps in 0u32..10_000) {
            let fee = Amount(a).mul_bps(Bps(bps)).unwrap();
            prop_assert!(fee.raw().abs() <= a.abs());
        }

        #[test]
        fn mul_bps_ceil_at_least_mul_bps(a in 0i64..1_000_000_000_000, bps in 0u32..10_000) {
            let truncated = Amount(a).mul_bps(Bps(bps)).unwrap();
            let ceiled = Amount(a).mul_bps_ceil(Bps(bps)).unwrap();
            prop_assert!(ceiled.raw() >= truncated.raw());
            prop_assert!(ceiled.raw() - truncated.raw() <= 1);
        }
    }
}
