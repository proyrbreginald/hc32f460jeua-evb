//! Hardware-independent classical CAN bit-timing calculation.
//!
//! HC32F460's `S_SEG_1` includes the synchronization segment, so one bit is
//! `time_seg1 + time_seg2` time quanta. The hardware also gives a programmed
//! prescaler of one a special, two-TQ-earlier sample point. This calculator
//! deliberately starts at an actual prescaler of two so the ordinary timing
//! formulas remain valid.

/// ISO 11898-1 classical CAN limit documented for HC32F460.
pub const MAX_CLASSIC_BITRATE: u32 = 1_000_000;

/// One valid HC32F460 classical CAN nominal bit timing.
///
/// All fields are actual time-quanta values, not the biased values stored in
/// the SBT register. Values returned by [`calculate`] satisfy the DDL's SBT
/// constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitTiming {
    /// CAN communication-clock divider (`2..=256`).
    pub prescaler: u32,
    /// Synchronization segment, propagation segment, and phase segment 1
    /// combined (`2..=65`).
    pub time_seg1: u32,
    /// Phase segment 2 (`1..=8`).
    pub time_seg2: u32,
    /// Synchronization jump width (`1..=8`).
    pub sjw: u32,
}

impl BitTiming {
    /// Number of time quanta in one nominal bit.
    pub const fn total_time_quanta(&self) -> u32 {
        self.time_seg1 + self.time_seg2
    }

    /// Actual nominal bitrate, rounded down to an integer number of bits/s.
    pub const fn actual_bitrate(&self, clock_hz: u32) -> u32 {
        let total_tq = self.time_seg1 as u64 + self.time_seg2 as u64;
        let divisor = match (self.prescaler as u64).checked_mul(total_tq) {
            Some(value) if value != 0 => value,
            _ => return 0,
        };
        (clock_hz as u64 / divisor) as u32
    }

    /// Actual sample point in permille, rounded to the nearest permille.
    pub const fn sample_point_permille(&self) -> u32 {
        let total_tq = self.time_seg1 as u64 + self.time_seg2 as u64;
        if total_tq == 0 {
            return 0;
        }
        ((self.time_seg1 as u64 * 1_000 + total_tq / 2) / total_tq) as u32
    }

    /// Absolute bitrate error in parts per million, rounded to the nearest ppm.
    ///
    /// Invalid zero inputs and arithmetic overflow return `u32::MAX`.
    pub const fn error_ppm(&self, clock_hz: u32, target_bitrate: u32) -> u32 {
        if target_bitrate == 0 {
            return u32::MAX;
        }

        let total_tq = self.time_seg1 as u64 + self.time_seg2 as u64;
        let divisor = match (self.prescaler as u64).checked_mul(total_tq) {
            Some(value) if value != 0 => value,
            _ => return u32::MAX,
        };
        let target_cycles = match (target_bitrate as u64).checked_mul(divisor) {
            Some(value) if value != 0 => value,
            _ => return u32::MAX,
        };
        let difference = abs_diff(clock_hz as u64, target_cycles);
        let (floor_ppm, remainder) = ppm_floor_and_remainder(difference, target_cycles);
        let rounded_ppm = if remainder >= target_cycles.div_ceil(2) {
            floor_ppm + 1
        } else {
            floor_ppm
        };

        if rounded_ppm > u32::MAX as u64 {
            u32::MAX
        } else {
            rounded_ppm as u32
        }
    }

    /// Encode this timing for the HC32F460 CAN SBT register.
    pub const fn register_value(&self) -> u32 {
        ((self.prescaler - 1) << 24)
            | ((self.sjw - 1) << 16)
            | ((self.time_seg2 - 1) << 8)
            | (self.time_seg1 - 2)
    }
}

/// Find the best HC32F460 classical CAN nominal bit timing.
///
/// `sample_point_permille` must be in `1..=999`, and `sjw` must be in
/// `1..=8`. Candidates are ordered by exact bitrate error, then exact sample
/// point error, then by the larger number of time quanta. `max_error_ppm` is
/// applied to the exact (unrounded) bitrate error.
pub const fn calculate(
    clock_hz: u32,
    target_bitrate: u32,
    target_sample_point_permille: u32,
    sjw: u32,
    max_error_ppm: u32,
) -> Option<BitTiming> {
    if clock_hz == 0
        || target_bitrate == 0
        || target_bitrate > MAX_CLASSIC_BITRATE
        || target_sample_point_permille == 0
        || target_sample_point_permille >= 1_000
        || sjw == 0
        || sjw > 8
    {
        return None;
    }

    let mut best = None;
    let mut best_bitrate_difference = 0u64;
    let mut best_divisor = 1u64;
    let mut best_sample_difference = 0u64;
    let mut best_total_tq = 0u64;

    // Actual prescaler one is intentionally excluded: PRESC=0 in SBT moves
    // this controller's sample point two TQ earlier than the normal formula.
    let mut prescaler = 2u32;
    while prescaler <= 256 {
        let mut time_seg2 = sjw;
        while time_seg2 <= 8 {
            let mut time_seg1 = time_seg2 + 1;
            if time_seg1 < 2 {
                time_seg1 = 2;
            }

            while time_seg1 <= 65 {
                let total_tq = (time_seg1 + time_seg2) as u64;
                let divisor = prescaler as u64 * total_tq;
                let target_cycles = target_bitrate as u64 * divisor;
                let bitrate_difference = abs_diff(clock_hz as u64, target_cycles);
                let within_hardware_rate = clock_hz as u64 <= MAX_CLASSIC_BITRATE as u64 * divisor;

                if within_hardware_rate
                    && within_error_limit(bitrate_difference, target_cycles, max_error_ppm)
                {
                    let desired_sample = target_sample_point_permille as u64 * total_tq;
                    let actual_sample = time_seg1 as u64 * 1_000;
                    let sample_difference = abs_diff(actual_sample, desired_sample);

                    let is_better = if best.is_none() {
                        true
                    } else {
                        // Compare the exact errors |clock / divisor - bitrate|
                        // without rounding either candidate's bitrate.
                        let candidate_rate = bitrate_difference * best_divisor;
                        let incumbent_rate = best_bitrate_difference * divisor;
                        if candidate_rate != incumbent_rate {
                            candidate_rate < incumbent_rate
                        } else {
                            // Likewise compare exact sample-point errors rather
                            // than their integer-permille representations.
                            let candidate_sample = sample_difference * best_total_tq;
                            let incumbent_sample = best_sample_difference * total_tq;
                            if candidate_sample != incumbent_sample {
                                candidate_sample < incumbent_sample
                            } else {
                                total_tq > best_total_tq
                            }
                        }
                    };

                    if is_better {
                        best = Some(BitTiming {
                            prescaler,
                            time_seg1,
                            time_seg2,
                            sjw,
                        });
                        best_bitrate_difference = bitrate_difference;
                        best_divisor = divisor;
                        best_sample_difference = sample_difference;
                        best_total_tq = total_tq;
                    }
                }

                time_seg1 += 1;
            }
            time_seg2 += 1;
        }
        prescaler += 1;
    }

    best
}

const fn abs_diff(lhs: u64, rhs: u64) -> u64 {
    lhs.abs_diff(rhs)
}

/// Compute `numerator * 1_000_000 / denominator` without constructing the
/// potentially overflowing product. The returned remainder belongs to that
/// scaled division.
const fn ppm_floor_and_remainder(numerator: u64, denominator: u64) -> (u64, u64) {
    let whole = numerator / denominator;
    let mut remainder = numerator % denominator;
    let mut fraction = 0u64;
    let mut digit = 0;

    while digit < 6 {
        // For candidates produced by `calculate`, denominator is at most
        // u32::MAX * (256 * 73), so this multiplication cannot overflow u64.
        remainder *= 10;
        fraction = fraction * 10 + remainder / denominator;
        remainder %= denominator;
        digit += 1;
    }

    (whole * 1_000_000 + fraction, remainder)
}

const fn within_error_limit(difference: u64, target_cycles: u64, limit_ppm: u32) -> bool {
    let (floor_ppm, remainder) = ppm_floor_and_remainder(difference, target_cycles);
    let limit_ppm = limit_ppm as u64;

    floor_ppm < limit_ppm || (floor_ppm == limit_ppm && remainder == 0)
}

#[cfg(test)]
mod tests {
    use super::{BitTiming, MAX_CLASSIC_BITRATE, calculate};

    const CANONICAL_500K: Option<BitTiming> = calculate(8_000_000, 500_000, 750, 2, 0);

    #[test]
    fn canonical_8mhz_500k_timing_and_sbt_encoding_are_stable() {
        let timing = CANONICAL_500K.unwrap();

        assert_eq!(timing.prescaler, 2);
        assert_eq!(timing.time_seg1, 6);
        assert_eq!(timing.time_seg2, 2);
        assert_eq!(timing.sjw, 2);
        assert_eq!(timing.total_time_quanta(), 8);
        assert_eq!(timing.actual_bitrate(8_000_000), 500_000);
        assert_eq!(timing.sample_point_permille(), 750);
        assert_eq!(timing.error_ppm(8_000_000, 500_000), 0);
        assert_eq!(timing.register_value(), 0x0101_0104);
    }

    #[test]
    fn common_bitrates_prefer_the_largest_equally_accurate_bit_time() {
        let cases = [
            (1_000_000, 1, 2, 3, 1),
            (250_000, 2, 2, 12, 4),
            (125_000, 2, 2, 24, 8),
            (100_000, 2, 4, 15, 5),
            (50_000, 2, 5, 24, 8),
        ];

        for (bitrate, sjw, prescaler, time_seg1, time_seg2) in cases {
            let timing = calculate(8_000_000, bitrate, 750, sjw, 0).unwrap();
            assert_eq!(timing.prescaler, prescaler, "{bitrate} bit/s");
            assert_eq!(timing.time_seg1, time_seg1, "{bitrate} bit/s");
            assert_eq!(timing.time_seg2, time_seg2, "{bitrate} bit/s");
            assert_eq!(timing.actual_bitrate(8_000_000), bitrate);
            assert_eq!(timing.sample_point_permille(), 750);
        }
    }

    #[test]
    fn rejects_invalid_inputs_and_unreachable_sjw() {
        assert_eq!(calculate(0, 500_000, 750, 1, 10_000), None);
        assert_eq!(calculate(8_000_000, 0, 750, 1, 10_000), None);
        assert_eq!(calculate(8_000_000, 500_000, 0, 1, 10_000), None);
        assert_eq!(calculate(8_000_000, 500_000, 1_000, 1, 10_000), None);
        assert_eq!(calculate(8_000_000, 500_000, 750, 0, 10_000), None);
        assert_eq!(calculate(8_000_000, 500_000, 750, 9, 10_000), None);
        assert_eq!(calculate(8_000_000, 500_000, 750, 8, 0), None);
        assert_eq!(calculate(8_000_000, 1_333_333, 667, 1, 0), None);
    }

    #[test]
    fn never_selects_an_actual_rate_above_the_classic_can_limit() {
        let timing = calculate(8_500_000, MAX_CLASSIC_BITRATE, 750, 1, 1_000_000).unwrap();
        assert!(timing.actual_bitrate(8_500_000) <= MAX_CLASSIC_BITRATE);
    }

    #[test]
    fn enforces_the_exact_bitrate_error_limit() {
        assert_eq!(calculate(8_000_000, 333_000, 750, 1, 1_000), None);

        let timing = calculate(8_000_000, 333_000, 750, 1, 1_002).unwrap();
        assert_eq!(timing.prescaler * timing.total_time_quanta(), 24);
        assert_eq!(timing.actual_bitrate(8_000_000), 333_333);
        assert_eq!(timing.error_ppm(8_000_000, 333_000), 1_001);
    }

    #[test]
    fn returned_values_obey_ddl_boundaries_and_exclude_prescaler_one() {
        let minimum = calculate(6_000_000, 1_000_000, 667, 1, 0).unwrap();
        assert_eq!(
            minimum,
            BitTiming {
                prescaler: 2,
                time_seg1: 2,
                time_seg2: 1,
                sjw: 1,
            }
        );

        let maximum = calculate(18_688_000, 1_000, 890, 8, 0).unwrap();
        assert_eq!(
            maximum,
            BitTiming {
                prescaler: 256,
                time_seg1: 65,
                time_seg2: 8,
                sjw: 8,
            }
        );
        assert_eq!(maximum.register_value(), 0xFF07_073F);

        for timing in [minimum, maximum, CANONICAL_500K.unwrap()] {
            assert!((2..=256).contains(&timing.prescaler));
            assert!((2..=65).contains(&timing.time_seg1));
            assert!((1..=8).contains(&timing.time_seg2));
            assert!((1..=8).contains(&timing.sjw));
            assert!(timing.time_seg1 >= timing.time_seg2 + 1);
            assert!(timing.time_seg2 >= timing.sjw);
        }
    }

    #[test]
    fn extreme_u32_inputs_do_not_overflow() {
        assert_eq!(calculate(u32::MAX, u32::MAX, 750, 1, u32::MAX), None);
        assert!(calculate(u32::MAX, MAX_CLASSIC_BITRATE, 750, 1, u32::MAX).is_some());

        let fastest_divider = BitTiming {
            prescaler: 2,
            time_seg1: 2,
            time_seg2: 1,
            sjw: 1,
        };
        assert_eq!(fastest_divider.error_ppm(u32::MAX, 1), u32::MAX);
        assert_eq!(calculate(u32::MAX, 1, 750, 1, u32::MAX), None);
    }
}
