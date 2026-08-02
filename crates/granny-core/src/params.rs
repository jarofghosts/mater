//! The eight hardware knobs, the five setting bits, and the plugin-only additions.
//!
//! Parameter values are kept in the *raw integer ranges the firmware stores them in* so the plugin
//! surface is one-to-one with the instrument: 0..=1023 for the 10-bit values, 0..=127 for the
//! 7-bit ones, 0..=255 for shift. Defaults are the firmware's `clearTo` table from `MEM.ino`.

use crate::curves::CurveMode;

/// Number of routing slots in the per-voice modulation matrix.
pub const MOD_SLOTS: usize = 3;

/// The eight knobs, in the order CLAP polyphonic modulation ids and the hardware's CC map use.
pub const KNOBS: usize = 8;

/// Full-scale span of each knob, indexed the same way as [`Expression::poly_mod`].
pub const KNOB_RANGES: [f32; KNOBS] = [
    1023.0, // rate
    127.0,  // crush
    127.0,  // attack
    127.0,  // release
    127.0,  // grain
    255.0,  // shift
    1023.0, // start
    1023.0, // end
];

/// The eight knobs and the setting bits.
#[derive(Clone, Debug, PartialEq)]
pub struct HwParams {
    /// 0..=1023. Playback rate in slice mode; a transpose in pitch mode (see [`Self::tuned`]).
    pub rate: u16,
    /// 0..=127. OR-mask bitcrush depth.
    pub crush: u16,
    /// 0..=127. Milliseconds *per envelope step*, not total attack time.
    pub attack: u16,
    /// 0..=127. Milliseconds per envelope step during release.
    pub release: u16,
    /// 0..=127. Grain size; zero switches the granular engine off.
    pub grain: u16,
    /// 0..=255. Grain shift, bipolar around 128.
    pub shift: u16,
    /// 0..=1023. Loop start.
    pub start: u16,
    /// 0..=1023. Loop end; 1000 and above means "the end of the sample".
    pub end: u16,

    /// The firmware's TUNED bit. On: notes set pitch and START sets the loop start. Off: notes
    /// select one of 60 slices and the RATE knob sets the pitch.
    pub tuned: bool,
    /// LEGATO: a new note on a channel that is already sounding retunes it instead of retriggering.
    pub legato: bool,
    /// REPEAT: loop between start and end instead of playing once.
    pub repeat: bool,
    /// SYNC: take grain and loop timing from the host transport rather than milliseconds.
    pub sync: bool,
    /// RANDOM SHIFT: flip the sign of the grain shift on roughly half of all control ticks.
    pub random_shift: bool,

    /// Notes keep sounding after note-off.
    pub hold: bool,
    /// Output level applied to every voice.
    pub level: f32,
    /// Pitch bend range in semitones. MPE's default is 48.
    pub bend_range: f32,
}

impl Default for HwParams {
    /// The firmware's `clearTo` defaults: RATE 877 (native rate), SHIFT 128 (no shift), END 1022,
    /// SETTING 13 (tuned, repeat and sync on; legato and random shift off).
    fn default() -> Self {
        Self {
            rate: 877,
            crush: 0,
            attack: 0,
            release: 0,
            grain: 0,
            shift: 128,
            start: 0,
            end: 1022,
            tuned: true,
            legato: false,
            repeat: true,
            sync: true,
            random_shift: false,
            hold: false,
            level: 0.5,
            bend_range: 48.0,
        }
    }
}

/// Which departures from the hardware are enabled. All default to "behave like the hardware".
#[derive(Clone, Debug, PartialEq)]
pub struct Fidelity {
    /// Whether curve maps reproduce the AVR's 16-bit overflow.
    pub curve_mode: CurveMode,
    /// Linearly interpolate between samples instead of dropping/repeating them.
    pub interpolate: bool,
    /// Floor every seek to a 512-byte SD block, as `WaveRP::seek` does.
    pub quantize_seeks: bool,
    /// Fade each grain in over this many milliseconds. Zero is the hardware's hard edge.
    pub grain_fade_ms: f32,
}

impl Default for Fidelity {
    fn default() -> Self {
        Self {
            curve_mode: CurveMode::HardwareExact,
            interpolate: false,
            quantize_seeks: true,
            grain_fade_ms: 0.0,
        }
    }
}

/// A per-voice modulation source. None of these exist on the hardware.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ModSource {
    #[default]
    None,
    /// MPE channel pressure or CLAP poly pressure, 0..1.
    Pressure,
    /// MPE slide (CC74) or CLAP poly brightness, 0..1.
    Slide,
    /// Note-on velocity, 0..1.
    Velocity,
    /// Per-channel pitch bend, -1..1.
    Bend,
    /// A value drawn once per note, 0..1.
    Random,
}

/// A per-voice modulation destination.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ModDest {
    #[default]
    None,
    Rate,
    Crush,
    Attack,
    Release,
    Grain,
    Shift,
    Start,
    End,
    Level,
    Pan,
}

impl ModDest {
    /// Full-scale span of the destination, so a depth of 1.0 always means "the whole range".
    pub fn range(self) -> f32 {
        match self {
            ModDest::Rate | ModDest::Start | ModDest::End => 1023.0,
            ModDest::Crush | ModDest::Attack | ModDest::Release | ModDest::Grain => 127.0,
            ModDest::Shift => 255.0,
            ModDest::Level | ModDest::Pan | ModDest::None => 1.0,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub struct ModSlot {
    pub source: ModSource,
    pub dest: ModDest,
    /// Bipolar depth, -1..1.
    pub depth: f32,
}

/// Per-voice expression state, fed by MPE and CLAP note expressions.
#[derive(Clone, Debug)]
pub struct Expression {
    /// Pitch offset in semitones from a CLAP tuning note expression.
    pub note_tuning: f32,
    /// Pitch bend, -1..1. Scaled by [`HwParams::bend_range`], and also a mod matrix source.
    pub bend: f32,
    pub pressure: f32,
    pub slide: f32,
    pub velocity: f32,
    /// Value drawn once at note-on for the Random mod source.
    pub random: f32,
    /// Note expression pan, -1..1.
    pub pan: f32,
    /// Note expression volume, used as a multiplier.
    pub gain: f32,
    /// CLAP polyphonic modulation offsets, normalised, one per knob in [`KNOB_RANGES`] order.
    pub poly_mod: [f32; KNOBS],
}

impl Default for Expression {
    fn default() -> Self {
        Self {
            note_tuning: 0.0,
            bend: 0.0,
            pressure: 0.0,
            slide: 0.0,
            velocity: 1.0,
            random: 0.0,
            pan: 0.0,
            gain: 1.0,
            poly_mod: [0.0; KNOBS],
        }
    }
}

impl Expression {
    /// Total pitch offset in semitones from every per-voice pitch source.
    pub fn pitch_offset(&self, bend_range: f32) -> f32 {
        self.note_tuning + self.bend * bend_range
    }
}

/// Parameter values after per-voice modulation has been applied.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub rate: u16,
    pub crush: u16,
    pub attack: u16,
    pub release: u16,
    pub grain: u16,
    pub shift: u16,
    pub start: u16,
    pub end: u16,
    pub level: f32,
    pub pan: f32,
}

fn offset(base: u16, amount: f32, max: u16) -> u16 {
    (base as f32 + amount).round().clamp(0.0, max as f32) as u16
}

/// Apply polyphonic modulation and the modulation matrix to the globals, for one voice.
pub fn resolve(p: &HwParams, mods: &[ModSlot], e: &Expression) -> Resolved {
    let m = &e.poly_mod;
    let mut r = Resolved {
        rate: offset(p.rate, m[0] * KNOB_RANGES[0], 1023),
        crush: offset(p.crush, m[1] * KNOB_RANGES[1], 127),
        attack: offset(p.attack, m[2] * KNOB_RANGES[2], 127),
        release: offset(p.release, m[3] * KNOB_RANGES[3], 127),
        grain: offset(p.grain, m[4] * KNOB_RANGES[4], 127),
        shift: offset(p.shift, m[5] * KNOB_RANGES[5], 255),
        start: offset(p.start, m[6] * KNOB_RANGES[6], 1023),
        end: offset(p.end, m[7] * KNOB_RANGES[7], 1023),
        level: p.level * e.gain,
        pan: e.pan,
    };

    for slot in mods {
        if slot.dest == ModDest::None || slot.source == ModSource::None || slot.depth == 0.0 {
            continue;
        }
        let source = match slot.source {
            ModSource::None => continue,
            ModSource::Pressure => e.pressure,
            ModSource::Slide => e.slide,
            ModSource::Velocity => e.velocity,
            ModSource::Bend => e.bend,
            ModSource::Random => e.random,
        };
        let amount = slot.depth * source * slot.dest.range();

        match slot.dest {
            ModDest::None => {}
            ModDest::Rate => r.rate = offset(r.rate, amount, 1023),
            ModDest::Crush => r.crush = offset(r.crush, amount, 127),
            ModDest::Attack => r.attack = offset(r.attack, amount, 127),
            ModDest::Release => r.release = offset(r.release, amount, 127),
            ModDest::Grain => r.grain = offset(r.grain, amount, 127),
            ModDest::Shift => r.shift = offset(r.shift, amount, 255),
            ModDest::Start => r.start = offset(r.start, amount, 1023),
            ModDest::End => r.end = offset(r.end, amount, 1023),
            ModDest::Level => r.level = (r.level + amount).clamp(0.0, 2.0),
            ModDest::Pan => r.pan = (r.pan + amount).clamp(-1.0, 1.0),
        }
    }

    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_firmware_clear_to_table() {
        let p = HwParams::default();
        assert_eq!((p.rate, p.crush, p.attack, p.release), (877, 0, 0, 0));
        assert_eq!((p.grain, p.shift, p.start, p.end), (0, 128, 0, 1022));
        // SETTING = 13 = 0b01101
        assert!(p.tuned && p.repeat && p.sync);
        assert!(!p.legato && !p.random_shift);
    }

    #[test]
    fn unrouted_slots_change_nothing() {
        let p = HwParams::default();
        let e = Expression::default();
        let mods = [ModSlot::default(); MOD_SLOTS];
        let r = resolve(&p, &mods, &e);
        assert_eq!(r.start, p.start);
        assert_eq!(r.crush, p.crush);
    }

    #[test]
    fn full_depth_covers_the_whole_destination_range() {
        let p = HwParams::default();
        let e = Expression {
            slide: 1.0,
            ..Default::default()
        };
        let mods = [ModSlot {
            source: ModSource::Slide,
            dest: ModDest::Start,
            depth: 1.0,
        }];
        assert_eq!(resolve(&p, &mods, &e).start, 1023);
    }

    #[test]
    fn negative_depth_and_clamping_behave() {
        let p = HwParams {
            crush: 64,
            ..Default::default()
        };
        let e = Expression {
            pressure: 1.0,
            ..Default::default()
        };
        let mods = [ModSlot {
            source: ModSource::Pressure,
            dest: ModDest::Crush,
            depth: -1.0,
        }];
        assert_eq!(resolve(&p, &mods, &e).crush, 0, "clamps at the bottom");
    }

    #[test]
    fn slots_accumulate_on_the_same_destination() {
        let p = HwParams {
            grain: 0,
            ..Default::default()
        };
        let e = Expression {
            pressure: 1.0,
            slide: 1.0,
            ..Default::default()
        };
        let mods = [
            ModSlot {
                source: ModSource::Pressure,
                dest: ModDest::Grain,
                depth: 0.25,
            },
            ModSlot {
                source: ModSource::Slide,
                dest: ModDest::Grain,
                depth: 0.25,
            },
        ];
        assert_eq!(resolve(&p, &mods, &e).grain, 64);
    }

    #[test]
    fn poly_modulation_offsets_are_normalised_to_each_knob_range() {
        let p = HwParams::default();
        let mut e = Expression::default();
        e.poly_mod[6] = 0.5; // start
        e.poly_mod[1] = 1.0; // crush
        let r = resolve(&p, &[], &e);
        assert_eq!(r.start, 512);
        assert_eq!(r.crush, 127);
    }

    #[test]
    fn poly_modulation_and_the_matrix_stack() {
        let p = HwParams {
            grain: 0,
            ..Default::default()
        };
        let mut e = Expression {
            pressure: 1.0,
            ..Default::default()
        };
        e.poly_mod[4] = 0.25;
        let mods = [ModSlot {
            source: ModSource::Pressure,
            dest: ModDest::Grain,
            depth: 0.25,
        }];
        // 127 * 0.25 rounds to 32, applied twice.
        assert_eq!(resolve(&p, &mods, &e).grain, 64);
    }

    #[test]
    fn pitch_offset_combines_bend_and_note_expression() {
        let e = Expression {
            note_tuning: 0.5,
            bend: 0.25,
            ..Default::default()
        };
        assert_eq!(e.pitch_offset(48.0), 0.5 + 12.0);
    }

    #[test]
    fn note_expression_volume_scales_the_level() {
        let p = HwParams::default();
        let e = Expression {
            gain: 0.5,
            ..Default::default()
        };
        let r = resolve(&p, &[], &e);
        assert!((r.level - p.level * 0.5).abs() < 1e-6);
    }
}
