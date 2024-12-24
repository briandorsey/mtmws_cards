#![no_std]

use defmt::*;
use derive_more::Shr;

/// A 12 bit value representing input from a knob or input jack's ADC
///
/// Normalized to the range -2048 to 2047 inclusive. Stored as i32 to give
/// room for integer math without needing allocations and the rp2040 is 32bit.
/// Conversions from/to this type saturate (clamp) - they stop at the min/max
/// values without giving errors. Before converting, raw internal value may be
/// outside of 12 bit range (allowing for math & accumulations, etc).
#[derive(
    Debug,
    Format,
    PartialEq,
    Copy,
    Clone,
    derive_more::Add,
    derive_more::Sub,
    derive_more::Mul,
    derive_more::Div,
)]
#[mul(forward)]
pub struct InputValue(i32);

// CONST values for min/max values? (12 bit limits)

impl InputValue {
    pub const MIN: i32 = -2_i32.pow(11);
    pub const MAX: i32 = 2_i32.pow(11) - 1;
    pub const CENTER: i32 = 0;
    pub const OFFSET: i32 = 2_i32.pow(11);

    pub fn new(value: i32) -> Self {
        InputValue(value)
    }

    /// Convert from u16 and offset value so center is at zero
    pub fn from_u16(value: u16) -> Self {
        let output = i32::from(value);
        Self(output - Self::OFFSET)
    }

    /// Convert from u16 and offset value so center is at zero, then invert
    pub fn from_u16_inverted(value: u16) -> Self {
        let output = i32::from(value);
        Self(output - Self::OFFSET) * InputValue::new(-1)
    }

    /// Saturating conversion into 11 bit safe u16 for output
    pub fn to_output(&self) -> u16 {
        // clamp self, divide by 2 (by shifting right) and convert to u16
        (self.to_clamped() + Self::OFFSET).shr(1) as u16
    }

    /// Saturating conversion into 11 bit safe u16 for output, inverted
    pub fn to_output_inverted(&self) -> u16 {
        2047_u16.saturating_sub(self.to_output())
    }

    pub fn to_clamped(&self) -> i32 {
        match self.0 {
            v if v > Self::MAX => Self::MAX,
            v if v < Self::MIN => Self::MIN,
            _ => self.0,
        }
    }
}

#[cfg(test)]
mod test {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::InputValue;

    #[test]
    fn test_input_value_basics() {
        assert_eq!(InputValue::MIN, -2048);
        assert_eq!(InputValue::MAX, 2047);
    }

    #[test]
    fn test_input_value_to_clamped() {
        // clamp to 12 bit values when inputs are above range
        assert_eq!(InputValue::from_u16(8000).to_clamped(), InputValue::MAX);
        assert_eq!(InputValue::from_u16(5000).to_clamped(), InputValue::MAX);
        assert_eq!(InputValue::from_u16(4096).to_clamped(), InputValue::MAX);
    }

    #[test]
    fn test_input_value_from() {
        assert_eq!(InputValue::from_u16(0).0, InputValue::MIN);
        assert_eq!(InputValue::from_u16(2048).0, 0);
        assert_eq!(InputValue::from_u16(4095).0, InputValue::MAX);
    }

    #[test]
    fn test_input_value_to_output() {
        assert_eq!(InputValue::from_u16(0).to_output(), 0);

        // clamp to 11 bit values in to_output() when inputs are above range
        assert_eq!(InputValue::from_u16(8000).to_output(), 2047_u16);
        assert_eq!(InputValue::from_u16(5000).to_output(), 2047_u16);
        assert_eq!(InputValue::from_u16(4096).to_output(), 2047_u16);

        let below_range = InputValue::from_u16(0) - InputValue::new(5000);
        assert_eq!(below_range.to_output(), 0_u16);
    }

    #[test]
    fn test_input_value_math() {
        assert_eq!(InputValue(123) * InputValue(1), InputValue(123));
    }
}
