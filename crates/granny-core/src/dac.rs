//! The output stage of the hardware, ported from `ISR(TIMER1_COMPB_vect)` in `WaveRP.cpp`.
//!
//! An 8-bit unsigned sample is shifted into a 12-bit DAC word, the crush mask is **OR'd** into it,
//! and the envelope multiplier is applied in 8.8 fixed point. The OR is the whole trick: crushing
//! *sets* low bits rather than clearing them, so it raises a noisy floor and drags quiet material
//! toward the DAC midpoint instead of simply reducing resolution.

use crate::tables::DAC_MID;

/// Raw `CRUSH` parameter (0..=127) -> the mask OR'd into the DAC word.
///
/// The firmware applies two separate shifts: `crush = getVar(CRUSH) << 1` in `UI.ino`, then
/// `_crush = _CRUSH << 2` inside `setCrush`. Net result is `<< 3`, spanning bits 3..=9 of a 12-bit
/// word, so at full crush the mask is 1016.
pub fn crush_mask(crush_param: u16) -> u16 {
    (crush_param & 0x7F) << 3
}

/// Produce one 12-bit DAC word.
///
/// `vol_mult == 0` is the firmware's sentinel for unity gain (see [`crate::envelope::vol_mult_offset`]).
pub fn dac_word(sample: u8, crush: u16, vol_mult: u8, vol_offset: u16) -> u16 {
    let mut data = ((sample as u16) << 4) | crush;
    if vol_mult != 0 {
        data = ((vol_mult as u32 * data as u32) >> 8) as u16;
        data = data.wrapping_add(vol_offset);
    }
    data
}

/// 12-bit DAC word -> normalised float, with the DAC midpoint as zero.
pub fn dac_to_f32(word: u16) -> f32 {
    (word as f32 - DAC_MID as f32) * (1.0 / DAC_MID as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_crush_sets_bits_three_through_nine() {
        assert_eq!(crush_mask(0), 0);
        assert_eq!(crush_mask(127), 1016);
        assert_eq!(crush_mask(1), 8);
    }

    #[test]
    fn crush_sets_bits_rather_than_truncating() {
        // A silent sample gains a large DC-ish offset under crush; a truncating bitcrusher would
        // leave it untouched.
        let clean = dac_word(0x80, 0, 0, 0);
        let crushed = dac_word(0x80, crush_mask(127), 0, 0);
        assert_eq!(clean, 2048);
        assert_eq!(crushed, 2048 | 1016);
        assert!(crushed > clean);
    }

    #[test]
    fn unity_gain_leaves_the_word_alone() {
        assert_eq!(dac_word(0xFF, 0, 0, 0), 0xFF << 4);
    }

    #[test]
    fn dac_word_never_exceeds_twelve_bits() {
        for sample in 0..=255u8 {
            for crush in [0u16, 63, 127] {
                for db in 0..=37u8 {
                    let (mult, offset) = crate::envelope::vol_mult_offset(db);
                    let word = dac_word(sample, crush_mask(crush), mult, offset);
                    assert!(
                        word <= 4095,
                        "sample {sample} crush {crush} db {db} -> {word}"
                    );
                }
            }
        }
    }

    #[test]
    fn midpoint_is_silence() {
        assert_eq!(dac_to_f32(DAC_MID), 0.0);
        assert!((dac_to_f32(4095) - 0.9995).abs() < 0.001);
        assert_eq!(dac_to_f32(0), -1.0);
    }
}
