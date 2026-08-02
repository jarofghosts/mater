//! Turning a (possibly fractional) MIDI note into a DAC sample rate.
//!
//! The hardware only ever looks a whole note up in a 49-entry table. Polyphonic MPE input means
//! notes are continuous, so [`PitchTable::Hardware`] interpolates *geometrically* between the
//! table's entries — matching it exactly on whole notes while giving sensible pitches in between —
//! and extrapolates by equal temperament past the ends, where the hardware simply clamps because
//! the AVR cannot clock its timer any faster.

use crate::scala::ScalaTuning;
use crate::tables::{
    NATIVE_NOTE, NATIVE_RATE, NOTE_SAMPLE_RATE, TABLE_FIRST_NOTE, TABLE_LAST_NOTE,
};

/// Which note-to-rate relationship to use.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum PitchTable {
    /// The firmware's rounded table. Whole notes match the hardware exactly.
    #[default]
    Hardware,
    /// Exact 12-tone equal temperament from 22050 Hz at note 59.
    EqualTemperament,
}

/// Index of the highest non-clamped entry in [`NOTE_SAMPLE_RATE`].
const TABLE_LAST_INDEX: usize = (TABLE_LAST_NOTE - TABLE_FIRST_NOTE) as usize;

/// Playback rate in Hz for a fractional MIDI note.
pub fn note_to_rate(note: f32, table: PitchTable) -> f32 {
    match table {
        PitchTable::EqualTemperament => NATIVE_RATE * exp2((note - NATIVE_NOTE) / 12.0),
        PitchTable::Hardware => {
            let index = note - TABLE_FIRST_NOTE as f32;
            if index <= 0.0 {
                NOTE_SAMPLE_RATE[0] as f32 * exp2(index / 12.0)
            } else if index >= TABLE_LAST_INDEX as f32 {
                NOTE_SAMPLE_RATE[TABLE_LAST_INDEX] as f32
                    * exp2((index - TABLE_LAST_INDEX as f32) / 12.0)
            } else {
                let lower = index.floor();
                let frac = index - lower;
                let a = NOTE_SAMPLE_RATE[lower as usize] as f32;
                let b = NOTE_SAMPLE_RATE[lower as usize + 1] as f32;
                // Geometric so the interpolation is linear in pitch, not in frequency.
                a * (b / a).powf(frac)
            }
        }
    }
}

fn exp2(x: f32) -> f32 {
    x.exp2()
}

/// Everything needed to place a note on the fractional-note axis and then find its rate.
#[derive(Clone, Debug)]
pub struct Tuning {
    pub table: PitchTable,
    /// Snap the final note to this many equal divisions of the octave. 24 gives quarter tones.
    pub snap_divisions: Option<u32>,
    /// A Scala scale, applied to the note number before any expression offset.
    ///
    /// Behind an `Arc` so the audio thread can rebuild a `Tuning` per block without allocating.
    pub scala: Option<std::sync::Arc<ScalaTuning>>,
    /// The fractional MIDI note the loaded sample already sounds at.
    ///
    /// This is what lets the output track the keyboard. The hardware has no equivalent: it plays
    /// whatever is on the card at whatever rate the DAC is clocked at, so the sounding pitch is
    /// whatever the sample happens to be. Here, playing note N transposes by `N - root` so note N
    /// is what is actually heard. [`NATIVE_NOTE`] means "play the sample untransposed", which is
    /// the hardware's behaviour.
    pub root_note: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            table: PitchTable::default(),
            snap_divisions: None,
            scala: None,
            root_note: NATIVE_NOTE,
        }
    }
}

impl Tuning {
    /// Resolve a note plus its accumulated pitch expression into a fractional MIDI note.
    ///
    /// Returns `None` when a Scala keyboard map leaves this key unmapped, which means the note
    /// should not sound at all.
    pub fn effective_note(&self, note: u8, offset_semitones: f32) -> Option<f32> {
        let base = match &self.scala {
            Some(scala) => scala.effective_note(note as i32)?,
            None => note as f32,
        };

        let shifted = base + offset_semitones;

        // Snapping happens after the offset so bends and slides land on scale steps too.
        Some(match self.snap_divisions {
            Some(divisions) if divisions > 0 => {
                let steps_per_semitone = divisions as f32 / 12.0;
                (shifted * steps_per_semitone).round() / steps_per_semitone
            }
            _ => shifted,
        })
    }

    /// Playback rate for a note, transposed so that the note is what actually sounds.
    ///
    /// Shifting by `root_note` rather than looking the note up directly is the whole trick: with a
    /// root of [`NATIVE_NOTE`] this is identical to the hardware, and with a detected root the
    /// output tracks the keyboard.
    pub fn rate_hz(&self, note: f32) -> f32 {
        note_to_rate(note - self.root_note + NATIVE_NOTE, self.table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_notes_match_the_hardware_table_exactly() {
        for (index, &expected) in NOTE_SAMPLE_RATE
            .iter()
            .enumerate()
            .take(TABLE_LAST_INDEX + 1)
        {
            let note = TABLE_FIRST_NOTE as f32 + index as f32;
            let rate = note_to_rate(note, PitchTable::Hardware);
            assert!(
                (rate - expected as f32).abs() < 0.5,
                "note {note}: got {rate}, want {expected}"
            );
        }
    }

    #[test]
    fn native_note_plays_at_the_native_rate() {
        assert!((note_to_rate(59.0, PitchTable::Hardware) - 22050.0).abs() < 0.5);
        assert!((note_to_rate(59.0, PitchTable::EqualTemperament) - 22050.0).abs() < 0.5);
    }

    #[test]
    fn quarter_tones_sit_between_their_neighbours() {
        let lower = note_to_rate(59.0, PitchTable::Hardware);
        let upper = note_to_rate(60.0, PitchTable::Hardware);
        let quarter = note_to_rate(59.5, PitchTable::Hardware);
        assert!(quarter > lower && quarter < upper);
        // Geometric interpolation: the quarter tone is the geometric mean of its neighbours.
        assert!((quarter - (lower * upper).sqrt()).abs() < 1.0);
    }

    #[test]
    fn octaves_double_the_rate_in_both_tables() {
        for table in [PitchTable::Hardware, PitchTable::EqualTemperament] {
            let ratio = note_to_rate(71.0, table) / note_to_rate(59.0, table);
            assert!((ratio - 2.0).abs() < 0.02, "{table:?} gave {ratio}");
        }
    }

    #[test]
    fn extrapolates_past_the_ends_of_the_table_instead_of_clamping() {
        // The hardware flattens above note 64; a plugin has no timer to run out of.
        let top = note_to_rate(64.0, PitchTable::Hardware);
        let higher = note_to_rate(76.0, PitchTable::Hardware);
        assert!((higher / top - 2.0).abs() < 0.01);

        let bottom = note_to_rate(23.0, PitchTable::Hardware);
        let lower = note_to_rate(11.0, PitchTable::Hardware);
        assert!((bottom / lower - 2.0).abs() < 0.01);
    }

    #[test]
    fn edo_snapping_quantises_bends_to_quarter_tones() {
        let tuning = Tuning {
            snap_divisions: Some(24),
            ..Default::default()
        };
        // A 40-cent bend snaps up to the quarter tone; a 20-cent bend snaps back down.
        assert_eq!(tuning.effective_note(60, 0.40), Some(60.5));
        assert_eq!(tuning.effective_note(60, 0.20), Some(60.0));
        assert_eq!(tuning.effective_note(60, -0.60), Some(59.5));
    }

    #[test]
    fn a_root_of_the_native_note_leaves_the_hardware_behaviour_alone() {
        let tuning = Tuning::default();
        assert_eq!(tuning.root_note, 59.0);
        for note in [23.0f32, 40.0, 59.0, 64.0] {
            assert_eq!(
                tuning.rate_hz(note),
                note_to_rate(note, PitchTable::Hardware)
            );
        }
    }

    #[test]
    fn a_detected_root_transposes_so_the_played_note_is_what_sounds() {
        // A sample that is itself a C4 (60). Playing C4 should give it back untransposed.
        let tuning = Tuning {
            table: PitchTable::EqualTemperament,
            root_note: 60.0,
            ..Default::default()
        };
        assert!((tuning.rate_hz(60.0) - NATIVE_RATE).abs() < 0.5);
        // An octave up doubles the rate, a fifth up is a 3:2 ratio.
        assert!((tuning.rate_hz(72.0) / tuning.rate_hz(60.0) - 2.0).abs() < 1e-3);
        assert!((tuning.rate_hz(67.0) / tuning.rate_hz(60.0) - 1.498307).abs() < 1e-3);
    }

    #[test]
    fn a_quarter_tone_above_the_root_is_a_quarter_tone_in_the_output() {
        let tuning = Tuning {
            table: PitchTable::EqualTemperament,
            root_note: 60.0,
            ..Default::default()
        };
        let ratio = tuning.rate_hz(60.5) / tuning.rate_hz(60.0);
        let cents = 1200.0 * ratio.log2();
        assert!((cents - 50.0).abs() < 0.01, "got {cents} cents");
    }

    #[test]
    fn a_fractional_root_is_compensated_exactly() {
        // A sample 30 cents sharp of A3.
        let tuning = Tuning {
            table: PitchTable::EqualTemperament,
            root_note: 57.3,
            ..Default::default()
        };
        // Asking for A3 has to pull it back down by exactly those 30 cents.
        let cents = 1200.0 * (tuning.rate_hz(57.0) / NATIVE_RATE).log2();
        assert!((cents - -30.0).abs() < 0.01, "got {cents} cents");
    }

    #[test]
    fn without_snapping_pitch_stays_continuous() {
        let tuning = Tuning::default();
        assert_eq!(tuning.effective_note(60, 0.37), Some(60.37));
    }
}
