#![cfg_attr(not(test), no_std)]

use core::fmt::Debug;
use core::ops::{Add, Div, Mul, Shr, Sub};

use defmt::*;

// Sample todos
//
// TODO: clean up to_output methods... flags, something? Think about the design.
// TODO: think about constructors, probably want to error when value out of range.

/// A 12 bit value representing input from a knob or input jack's ADC
///
/// Normalized to the range -2048 to 2047 inclusive. Stored as i32 to give
/// room for integer math without needing allocations and the rp2040 is 32bit.
/// Conversions from this type saturate (clamp) - they stop at the min/max
/// values without giving errors. Before converting, raw internal value will be
/// outside of 12 bit range (allowing for math & accumulations, etc).
///
/// Values are smoothed over recent updates (count based on `ACCUM_BITS`).
#[derive(Format, PartialEq, Copy, Clone, PartialOrd)]
pub struct Sample {
    accumulated_raw: i32,
    inverted_source: bool,
}

impl Debug for Sample {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::write!(
            f,
            "InputValue::new({}, {})",
            self.accumulated_raw >> Self::ACCUM_BITS,
            self.inverted_source,
        )
    }
}

impl Sample {
    // CONST for min/max values (12 bit limits, 11 on each positive/negative)
    pub const MIN: i32 = -2_i32.pow(11);
    pub const MAX: i32 = 2_i32.pow(11) - 1;
    pub const CENTER: i32 = 0;
    pub const OFFSET: i32 = 2_i32.pow(11);
    const ACCUM_BITS: u8 = 3;

    /// New `InputValue` from i32
    ///
    /// Values are expected to already be 12bit (-2048..2048), but this
    /// is not checked.
    pub fn new(raw_value: i32, invert: bool) -> Self {
        Sample {
            accumulated_raw: match invert {
                false => raw_value << Self::ACCUM_BITS,
                true => -raw_value << Self::ACCUM_BITS,
            },
            inverted_source: invert,
        }
    }

    /// New `InputValue` from u16 and offset value so center is at zero
    ///
    /// Values are expected to already be 12bit (0..4096), but this
    /// is not checked.
    pub fn from_u16(value: u16, invert: bool) -> Self {
        let mut output = i32::from(value);
        output -= Self::OFFSET;
        Self::new(output, invert)
    }

    /// Update with new value
    pub fn update(&mut self, value: u16) {
        let mut value = i32::from(value);
        value -= Self::OFFSET;
        if self.inverted_source {
            value = -value;
        }
        // first-order infinite impulse response filter, logic from:
        // https://electronics.stackexchange.com/a/176740
        self.accumulated_raw =
            (self.accumulated_raw - (self.accumulated_raw >> Self::ACCUM_BITS)) + value;
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

    /// Saturating conversion into 11 bit safe u16 for output, absolute value.
    pub fn to_output_abs(&self) -> u16 {
        // clamp self, take absolute value, clamp to max (negative i values
        // are one larger than positive), and convert to u16
        (self.to_clamped().abs()).min(Self::MAX) as u16
    }

    /// Saturating conversion into 11 bit safe u16 for output, inverted
    pub fn to_output_abs_inverted(&self) -> u16 {
        2047_u16.saturating_sub(self.to_output_abs())
    }

    pub fn to_clamped(&self) -> i32 {
        (self.accumulated_raw >> Self::ACCUM_BITS).clamp(Self::MIN, Self::MAX)
    }

    pub fn to_inverted(&self) -> Self {
        Self::new(-self.accumulated_raw, self.inverted_source)
    }

    pub fn abs(self) -> Self {
        // not expecting values to ever hit i32::MIN, but saturating, just in case
        Self::new(self.to_clamped().saturating_abs(), self.inverted_source)
    }

    pub fn scale(&self, other: Self) -> Self {
        ((self.to_clamped() * other.to_clamped()) / Self::MAX).into()
    }

    pub fn scale_inverted(&self, other: Self) -> Self {
        ((self.to_clamped() * (Self::MAX - other.to_clamped())) / Self::MAX).into()
    }
}

impl From<i32> for Sample {
    fn from(value: i32) -> Self {
        Self::new(value, false)
    }
}
impl From<i16> for Sample {
    fn from(value: i16) -> Self {
        Self::new(value.into(), false)
    }
}

impl Add for Sample {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self::Output {
        self.accumulated_raw += rhs.accumulated_raw;
        self
    }
}

impl Sub for Sample {
    type Output = Self;

    fn sub(mut self, rhs: Self) -> Self::Output {
        self.accumulated_raw -= rhs.accumulated_raw;
        self
    }
}

impl Mul for Sample {
    type Output = Self;

    fn mul(mut self, rhs: Self) -> Self::Output {
        self.accumulated_raw = ((self.accumulated_raw >> Self::ACCUM_BITS)
            * (rhs.accumulated_raw >> Self::ACCUM_BITS))
            << Self::ACCUM_BITS;
        self
    }
}

impl Mul<i32> for Sample {
    type Output = Self;

    fn mul(mut self, rhs: i32) -> Self::Output {
        self.accumulated_raw =
            ((self.accumulated_raw >> Self::ACCUM_BITS) * rhs) << Self::ACCUM_BITS;
        self
    }
}

impl Div<i32> for Sample {
    type Output = Self;

    fn div(mut self, rhs: i32) -> Self::Output {
        self.accumulated_raw =
            ((self.accumulated_raw >> Self::ACCUM_BITS) / rhs) << Self::ACCUM_BITS;
        self
    }
}

/// `JackValue` represents input values from a jack when a cable is plugged.
///
/// This struct expects both `raw` and `probe` values to be updated regularly.
/// When a value is requested, it only returns a value when a cable is
/// connected.
///
/// When the Computer module's normalization probe is enabled, all jacks
/// recieve a fixed voltage only when nothing is plugged into them. The voltage
/// difference between no cable and the probe should be a consistent value
/// significantly above zero. When a cable is plugged in there should be no
/// difference when the probe is enabled. The logic relies on both values to
/// be smoothed to avoid false negatives from short term voltages on the cable
/// which happen to have the right voltage difference between them from a single
/// sample.
#[derive(Format, Clone)]
pub struct JackSample {
    pub raw: Sample,
    pub probe: Sample,
}

// TODO: implement probe logic
impl JackSample {
    pub fn new(raw: Sample, probe: Sample) -> JackSample {
        JackSample { raw, probe }
    }

    pub fn plugged_value(&self) -> Option<&Sample> {
        let mut diff = self.probe.accumulated_raw - self.raw.accumulated_raw;
        diff >>= Sample::ACCUM_BITS;
        // determined through testing my unit, may need adjusting
        if diff > 300 {
            None
        } else {
            Some(&self.raw)
        }
    }
}

#[cfg(test)]
mod test {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::Sample;

    #[test]
    fn test_input_value_basics() {
        assert_eq!(Sample::MIN, -2048);
        assert_eq!(Sample::MAX, 2047);
    }

    #[test]
    fn test_input_value_to_clamped() {
        // clamp to 12 bit values when inputs are above range
        assert_eq!(Sample::from_u16(8000, false).to_clamped(), Sample::MAX);
        assert_eq!(Sample::from_u16(5000, false).to_clamped(), Sample::MAX);
        assert_eq!(Sample::from_u16(4096, false).to_clamped(), Sample::MAX);
    }

    #[test]
    fn test_input_value_from() {
        assert_eq!(Sample::from_u16(0, false).to_clamped(), Sample::MIN);
        assert_eq!(Sample::from_u16(2048, false).to_clamped(), 0);
        assert_eq!(Sample::from_u16(4095, false).to_clamped(), Sample::MAX);
    }

    #[test]
    fn test_input_value_to_output() {
        assert_eq!(Sample::new(Sample::CENTER, false).to_output(), 1024_u16);

        // output values are half of input (11 bit from 12 bit)
        assert_eq!(Sample::from_u16(0, false).to_output(), 0);
        assert_eq!(Sample::from_u16(2_u16, false).to_output(), 1_u16);
        assert_eq!(Sample::from_u16(1024_u16, false).to_output(), 512_u16);
        assert_eq!(Sample::from_u16(2048_u16, false).to_output(), 1024_u16);

        // clamp to 11 bit values in to_output() when inputs are above range
        assert_eq!(Sample::from_u16(8000, false).to_output(), 2047_u16);
        assert_eq!(Sample::from_u16(5000, false).to_output(), 2047_u16);
        assert_eq!(Sample::from_u16(4096, false).to_output(), 2047_u16);

        let below_range = Sample::from_u16(0, false) - Sample::new(5000, false);
        assert_eq!(below_range.to_output(), 0_u16);
    }

    #[test]
    fn test_input_value_inverted_to_output() {
        assert_eq!(Sample::new(Sample::CENTER, true).to_output(), 1024_u16);

        // output values are half of input (11 bit from 12 bit)
        assert_eq!(Sample::from_u16(0, true).to_output(), 2047);
        // assert_eq!(InputValue::from_u16(2_u16, true).to_output(), 2046_u16);
        assert_eq!(Sample::from_u16(1024_u16, true).to_output(), 1536_u16);
        assert_eq!(Sample::from_u16(2047_u16, true).to_output(), 1024_u16);

        // clamp to 11 bit values in to_output() when inputs are above range
        assert_eq!(Sample::from_u16(8000, true).to_output(), 0_u16);
        assert_eq!(Sample::from_u16(5000, true).to_output(), 0_u16);
        assert_eq!(Sample::from_u16(4096, true).to_output(), 0_u16);

        let below_range = Sample::from_u16(0, true) - Sample::new(5000, true);
        assert_eq!(below_range.to_output(), 2047_u16);
    }

    #[test]
    fn test_input_value_math() {
        assert_eq!(
            Sample::new(123, false) + Sample::new(456, false),
            Sample::new(579, false)
        );

        assert_eq!(Sample::new(123, false) * 1, Sample::new(123, false));
        assert_eq!(Sample::new(123, false) * 2, Sample::new(246, false));
        assert_eq!(Sample::new(123, false) * -1, Sample::new(-123, false));

        #[allow(clippy::erasing_op)]
        let expected = Sample::new(123, false) * 0;
        assert_eq!(expected, Sample::new(0, false));

        // division
        assert_eq!(Sample::new(123, false) / 1, Sample::new(123, false));
        assert_eq!(Sample::new(240, false) / 2, Sample::new(120, false));
        assert_eq!(Sample::new(123, false) / -1, Sample::new(-123, false));
    }
}
