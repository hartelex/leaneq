#![no_std]

/// Maximum number of bands without heap allocation.
pub const MAX_BANDS: usize = 16;
const DENORMAL_THRESHOLD: f32 = 1.0e-20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FilterType {
    Bypass,
    #[default]
    Peaking,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
}

#[derive(Clone, Copy, Debug)]
pub struct BiquadCoefficients {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl BiquadCoefficients {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };
}

impl Default for BiquadCoefficients {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Coefficient and state for one stereo band.
#[derive(Clone, Copy, Debug)]
pub struct StereoBiquad {
    pub coeffs: BiquadCoefficients,
    pub left_z1: f32,
    pub left_z2: f32,
    pub right_z1: f32,
    pub right_z2: f32,
    pub enabled: bool,
}

impl Default for StereoBiquad {
    fn default() -> Self {
        Self {
            coeffs: BiquadCoefficients::IDENTITY,
            left_z1: 0.0,
            left_z2: 0.0,
            right_z1: 0.0,
            right_z2: 0.0,
            enabled: false,
        }
    }
}

impl StereoBiquad {
    #[inline(always)]
    fn sanitize(value: f32) -> f32 {
        if value.abs() < DENORMAL_THRESHOLD || !value.is_finite() {
            0.0
        } else {
            value
        }
    }

    #[inline(always)]
    fn process_channel(input: f32, coeffs: &BiquadCoefficients, z1: &mut f32, z2: &mut f32) -> f32 {
        let output = coeffs.b0 * input + *z1;
        *z1 = coeffs.b1 * input - coeffs.a1 * output + *z2;
        *z2 = coeffs.b2 * input - coeffs.a2 * output;
        Self::sanitize(output)
    }

    #[inline(always)]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled {
            return (Self::sanitize(left), Self::sanitize(right));
        }

        let left_output =
            Self::process_channel(left, &self.coeffs, &mut self.left_z1, &mut self.left_z2);
        let right_output =
            Self::process_channel(right, &self.coeffs, &mut self.right_z1, &mut self.right_z2);
        (left_output, right_output)
    }

    pub fn reset(&mut self) {
        self.left_z1 = 0.0;
        self.left_z2 = 0.0;
        self.right_z1 = 0.0;
        self.right_z2 = 0.0;
    }

    pub fn set_coefficients(&mut self, coeffs: BiquadCoefficients) {
        self.coeffs = coeffs;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EqualizerChain {
    pub bands: [StereoBiquad; MAX_BANDS],
    pub preamp_db: f32,
    pub preamp_linear: f32,
    pub bypass: bool,
}

impl Default for EqualizerChain {
    fn default() -> Self {
        Self {
            bands: [StereoBiquad::default(); MAX_BANDS],
            preamp_db: 0.0,
            preamp_linear: 1.0,
            bypass: false,
        }
    }
}

impl EqualizerChain {
    #[inline(always)]
    pub fn preamp_linear(db: f32) -> f32 {
        libm::powf(10.0, db / 20.0)
    }

    #[inline(always)]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.bypass {
            return (left, right);
        }

        let gain = self.preamp_linear;
        let mut left = StereoBiquad::sanitize(left * gain);
        let mut right = StereoBiquad::sanitize(right * gain);

        for band in &mut self.bands {
            let (new_left, new_right) = band.process(left, right);
            left = new_left;
            right = new_right;
        }
        (left, right)
    }

    pub fn reset(&mut self) {
        for band in &mut self.bands {
            band.reset();
        }
    }
}

/// Calculates normalized RBJ coefficients in f64 and stores f32 results.
pub fn calculate_coefficients(
    filter_type: FilterType,
    sample_rate: f32,
    frequency: f32,
    gain_db: f32,
    q: f32,
) -> BiquadCoefficients {
    if !sample_rate.is_finite()
        || sample_rate <= 0.0
        || !frequency.is_finite()
        || frequency <= 0.0
        || !q.is_finite()
        || q <= 0.0
    {
        return BiquadCoefficients::IDENTITY;
    }

    let sr = sample_rate as f64;
    let f = frequency as f64;
    let q = q as f64;
    let a = libm::pow(10.0, gain_db as f64 / 40.0);
    let omega = 2.0 * core::f64::consts::PI * f / sr;
    let sin = libm::sin(omega);
    let cos = libm::cos(omega);
    let alpha = sin / (2.0 * q);

    let (mut b0, mut b1, mut b2, a0, mut a1, mut a2);

    match filter_type {
        FilterType::Bypass => {
            return BiquadCoefficients::IDENTITY;
        }
        FilterType::Peaking => {
            b0 = 1.0 + alpha * a;
            b1 = -2.0 * cos;
            b2 = 1.0 - alpha * a;
            a0 = 1.0 + alpha / a;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha / a;
        }
        FilterType::LowShelf => {
            let two_sqrt_a_alpha = 2.0 * libm::sqrt(a) * alpha;
            b0 = a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha);
            b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
            b2 = a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha);
            a0 = (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha;
            a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
            a2 = (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha;
        }
        FilterType::HighShelf => {
            let two_sqrt_a_alpha = 2.0 * libm::sqrt(a) * alpha;
            b0 = a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha);
            b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
            b2 = a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha);
            a0 = (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha;
            a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
            a2 = (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha;
        }
        FilterType::LowPass => {
            b0 = (1.0 - cos) / 2.0;
            b1 = 1.0 - cos;
            b2 = (1.0 - cos) / 2.0;
            a0 = 1.0 + alpha;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha;
        }
        FilterType::HighPass => {
            b0 = (1.0 + cos) / 2.0;
            b1 = -(1.0 + cos);
            b2 = (1.0 + cos) / 2.0;
            a0 = 1.0 + alpha;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha;
        }
    }

    if !a0.is_finite() || a0.abs() < 1.0e-12 {
        return BiquadCoefficients::IDENTITY;
    }

    b0 /= a0;
    b1 /= a0;
    b2 /= a0;
    a1 /= a0;
    a2 /= a0;

    BiquadCoefficients {
        b0: b0 as f32,
        b1: b1 as f32,
        b2: b2 as f32,
        a1: a1 as f32,
        a2: a2 as f32,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::f64::consts::PI;

    fn rms_db(filter_type: FilterType, sample_rate: f32, frequency: f32, gain: f32, q: f32) -> f32 {
        rms_db_with_cutoff(filter_type, sample_rate, frequency, frequency, gain, q)
    }

    fn rms_db_with_cutoff(
        filter_type: FilterType,
        sample_rate: f32,
        cutoff: f32,
        measured_frequency: f32,
        gain: f32,
        q: f32,
    ) -> f32 {
        let coeffs = calculate_coefficients(filter_type, sample_rate, cutoff, gain, q);
        let mut filter = StereoBiquad::default();
        filter.enabled = true;
        filter.set_coefficients(coeffs);

        const BLOCK: usize = 20_000;
        const SETTLE: usize = 2_000;
        let mut input_sum = 0.0_f64;
        let mut output_sum = 0.0_f64;
        let mut count = 0usize;

        for n in 0..BLOCK {
            let phase = 2.0 * PI * measured_frequency as f64 * n as f64 / sample_rate as f64;
            let input = libm::sin(phase) as f32;
            let (output, _) = filter.process(input, input);
            if n >= SETTLE {
                input_sum += (input as f64) * (input as f64);
                output_sum += (output as f64) * (output as f64);
                count += 1;
            }
        }

        let input_rms = libm::sqrt(input_sum / count as f64);
        let output_rms = libm::sqrt(output_sum / count as f64);
        if input_rms > 0.0 && output_rms > 0.0 {
            20.0 * libm::log10(output_rms / input_rms) as f32
        } else {
            -200.0
        }
    }

    #[test]
    fn peaking_has_expected_gain_at_center() {
        let db = rms_db(FilterType::Peaking, 48_000.0, 1_000.0, 6.0, 1.0);
        assert!((db - 6.0).abs() < 0.2, "peaking response was {db} dB");
    }

    #[test]
    fn peaking_has_expected_cut_at_center() {
        let db = rms_db(FilterType::Peaking, 48_000.0, 1_000.0, -6.0, 1.0);
        assert!((db - (-6.0)).abs() < 0.2, "peaking cut was {db} dB");
    }

    #[test]
    fn bypass_is_identity() {
        let mut chain = EqualizerChain::default();
        chain.bypass = true;
        assert_eq!(chain.process(0.25, -0.75), (0.25, -0.75));
    }

    #[test]
    fn denormals_are_suppressed() {
        assert_eq!(StereoBiquad::sanitize(1.0e-30), 0.0);
    }

    #[test]
    fn low_pass_reduces_high_frequency() {
        let low = rms_db_with_cutoff(FilterType::LowPass, 48_000.0, 1_000.0, 1_000.0, 0.0, 0.707);
        let high = rms_db_with_cutoff(FilterType::LowPass, 48_000.0, 1_000.0, 12_000.0, 0.0, 0.707);
        assert!(low > -4.0, "low-frequency response was {low} dB");
        assert!(high < -10.0, "high-frequency response was {high} dB");
        assert!(low > high + 10.0);
    }
}
