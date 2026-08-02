//! Parameter curve mapping, ported from `curveMap`/`map16`/`valueToSampleRate` in `UI.ino`.
//!
//! # The overflow
//!
//! `map16` is written against `uint16_t`:
//!
//! ```c
//! uint16_t map16(uint16_t x, uint16_t in_min, uint16_t in_max, uint16_t out_min, uint16_t out_max)
//! { return (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min; }
//! ```
//!
//! On AVR `int` is 16 bits, so `uint16_t` promotes to a 16-bit `unsigned int` and the multiply
//! wraps. Several segments of the 2.5 shift curve exceed 65535 and therefore fold back on the real
//! instrument: the shift knob is genuinely non-monotonic in its outer regions, and grain size folds
//! at the very top of its travel (parameter 127 gives 2127 ms, not 4000 ms).
//!
//! [`CurveMode::HardwareExact`] reproduces the fold. [`CurveMode::Extended`] does the same
//! arithmetic in 32 bits, giving the curve the firmware's tables were clearly drawn to describe.

use crate::tables::*;

/// Whether curve maps reproduce the AVR's 16-bit overflow.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum CurveMode {
    /// Bit-exact with the hardware, including the `uint16_t` wraparound. The default.
    #[default]
    HardwareExact,
    /// 32-bit arithmetic: smooth, monotonic curves that follow the firmware's breakpoint tables.
    Extended,
}

/// `map16` from the firmware. See the module docs for why `mode` exists.
pub fn map16(x: u16, in_min: u16, in_max: u16, out_min: u16, out_max: u16, mode: CurveMode) -> u16 {
    if in_max == in_min {
        return out_min;
    }
    let span = in_max.wrapping_sub(in_min);
    let rise = out_max.wrapping_sub(out_min);
    let run = x.wrapping_sub(in_min);

    match mode {
        CurveMode::HardwareExact => {
            // Deliberately 16-bit: this is the AVR's behaviour, wraparound included.
            let scaled = run.wrapping_mul(rise);
            (scaled / span).wrapping_add(out_min)
        }
        CurveMode::Extended => {
            let scaled = run as u32 * rise as u32;
            ((scaled / span as u32) as u16).wrapping_add(out_min)
        }
    }
}

/// `curveMap` from the firmware: find the bracketing segment, then linearly interpolate.
///
/// Matches the firmware's first-match-wins loop (a value landing exactly on a breakpoint uses the
/// segment *below* it) and its fall-through defaults when nothing matches.
pub fn curve_map(value: u8, points_in: &[u16], points_out: &[u16], mode: CurveMode) -> u16 {
    debug_assert_eq!(points_in.len(), points_out.len());
    let value = value as u16;

    let (mut in_min, mut in_max, mut out_min, mut out_max) = (0u16, 255u16, 0u16, 255u16);
    for i in 0..points_in.len() - 1 {
        if value >= points_in[i] && value <= points_in[i + 1] {
            in_min = points_in[i];
            in_max = points_in[i + 1];
            out_min = points_out[i];
            out_max = points_out[i + 1];
            break;
        }
    }

    map16(value, in_min, in_max, out_min, out_max, mode)
}

/// Grain size in milliseconds for a raw `LOOP_LENGTH` parameter (0..=127).
///
/// Zero means the granular engine is off and the sample just plays through.
pub fn grain_ms(loop_length: u16, mode: CurveMode) -> u16 {
    // The firmware passes `ll << 1` as a uint8_t, so 0..=127 becomes 0..=254.
    let value = ((loop_length & 0x7F) << 1) as u8;
    curve_map(value, &GRAIN_MAP_IN, &GRAIN_MAP_OUT, mode)
}

/// Signed grain shift in bytes for a raw `SHIFT_SPEED` parameter (0..=255).
///
/// The curve has a dead zone around the centre (raw 110..=146 all map to zero shift).
pub fn shift_bytes(shift: u16, mode: CurveMode) -> i32 {
    let value = (shift & 0xFF) as u8;
    curve_map(value, &SHIFT_MAP_IN, &SHIFT_MAP_OUT, mode) as i32 - SHIFT_CENTRE
}

/// Grain length in 24-ppq clock ticks when clock sync is on.
pub fn sync_grain_ticks(loop_length: u16) -> u16 {
    USEFUL_LENGTHS[((loop_length >> 3) as usize).min(USEFUL_LENGTHS.len() - 1)]
}

/// Loop length in 24-ppq clock ticks when clock sync is on.
pub fn sync_end_ticks(end: u16) -> u16 {
    USEFUL_LENGTHS[(((end >> 6) + 1) as usize).min(USEFUL_LENGTHS.len() - 1)]
}

/// `valueToSampleRate`: the RATE knob (0..=1023) mapped to a DAC rate in Hz.
///
/// In tuned mode the knob snaps to the semitone table; in free-run mode it interpolates across ten
/// substeps between adjacent table entries.
pub fn value_to_sample_rate(value: u16, tuned: bool) -> u32 {
    // `myMap(x, in_max, out_max)` == `x * out_max / in_max`, done in 32-bit `long` on AVR.
    let pitch = (value as u32 * 420 / 1023) as u16;
    let index = (pitch / 10) as usize;

    if tuned {
        NOTE_SAMPLE_RATE[index] as u32
    } else {
        let base = NOTE_SAMPLE_RATE[index];
        let next = NOTE_SAMPLE_RATE[index + 1];
        let step = (next - base) / 10;
        (base + (pitch % 10) * step) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_knob_default_is_native_rate() {
        // 877 is the firmware's `clearTo` default for RATE.
        assert_eq!(value_to_sample_rate(877, true), 22050);
        assert_eq!(value_to_sample_rate(877, false), 22050);
    }

    #[test]
    fn rate_knob_spans_the_note_table() {
        assert_eq!(value_to_sample_rate(0, true), 2772);
        assert_eq!(value_to_sample_rate(1023, true), 29480);
    }

    #[test]
    fn free_run_rate_interpolates_between_table_entries() {
        // pitch = 361 -> index 36, remainder 1 -> 22050 + (23420-22050)/10
        assert_eq!(value_to_sample_rate(880, false), 22050 + 137);
        // Tuned mode ignores the remainder entirely.
        assert_eq!(value_to_sample_rate(880, true), 22050);
    }

    #[test]
    fn shift_centre_is_a_dead_zone() {
        for raw in 110..=146 {
            assert_eq!(shift_bytes(raw, CurveMode::HardwareExact), 0, "raw = {raw}");
            assert_eq!(shift_bytes(raw, CurveMode::Extended), 0, "raw = {raw}");
        }
        // The firmware default of 128 sits inside the dead zone.
        assert_eq!(shift_bytes(128, CurveMode::HardwareExact), 0);
    }

    #[test]
    fn shift_extremes_reach_the_full_range() {
        assert_eq!(shift_bytes(0, CurveMode::Extended), -SHIFT_CENTRE);
        assert_eq!(shift_bytes(255, CurveMode::Extended), 32000 - SHIFT_CENTRE);
    }

    #[test]
    fn hardware_shift_curve_folds_where_the_avr_overflows() {
        // Values computed against a C transcription of map16 using 16-bit arithmetic.
        assert_eq!(
            shift_bytes(17, CurveMode::HardwareExact),
            648 - SHIFT_CENTRE
        );
        assert_eq!(shift_bytes(17, CurveMode::Extended), 2833 - SHIFT_CENTRE);
        assert_eq!(
            shift_bytes(255, CurveMode::HardwareExact),
            28255 - SHIFT_CENTRE
        );
        // Segments that do not overflow must agree between the two modes.
        for raw in [0u16, 34, 68, 102, 119, 153, 170, 187, 204, 221] {
            assert_eq!(
                shift_bytes(raw, CurveMode::HardwareExact),
                shift_bytes(raw, CurveMode::Extended),
                "raw = {raw}"
            );
        }
    }

    #[test]
    fn grain_curve_hits_its_breakpoints() {
        // Breakpoint inputs are on the doubled scale, so halve them to get parameter values.
        assert_eq!(grain_ms(0, CurveMode::Extended), 0);
        assert_eq!(grain_ms(40, CurveMode::Extended), 100); // in 80 -> out 100
        assert_eq!(grain_ms(80, CurveMode::Extended), 1000); // in 160 -> out 1000
        assert_eq!(grain_ms(110, CurveMode::Extended), 2000); // in 220 -> out 2000
                                                              // The parameter tops out at 127, i.e. curve input 254, just short of the 4000 ms endpoint.
        assert_eq!(grain_ms(127, CurveMode::Extended), 3942);
    }

    #[test]
    fn grain_curve_only_folds_at_the_very_top() {
        for raw in 0..=126u16 {
            assert_eq!(
                grain_ms(raw, CurveMode::HardwareExact),
                grain_ms(raw, CurveMode::Extended),
                "raw = {raw}"
            );
        }
        // At full travel the multiply wraps and grain size collapses from 3942 ms to 2070 ms.
        assert_eq!(grain_ms(127, CurveMode::HardwareExact), 2070);
    }

    #[test]
    fn sync_divisions_come_from_the_useful_lengths_table() {
        assert_eq!(sync_grain_ticks(0), 0);
        assert_eq!(sync_grain_ticks(48), 24); // a quarter note
        assert_eq!(sync_grain_ticks(127), 768);
        assert_eq!(sync_end_ticks(0), 1);
        assert_eq!(sync_end_ticks(1023), 24000);
    }
}
