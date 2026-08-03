//! The plugin's parameter surface.
//!
//! The eight knobs keep the hardware's raw integer ranges (0..=1023, 0..=127, 0..=255) so that
//! automation lines up one-to-one with the instrument and so preset values are directly comparable
//! with a real microGranny. Everything the hardware expresses through button combinations becomes
//! an ordinary parameter, and a handful of parameters exist that the hardware has no way to offer
//! at all: voice count, tuning, and the modulation matrix.

use granny_core::curves::{grain_ms, shift_bytes, sync_end_ticks, sync_grain_ticks, CurveMode};
use granny_core::dac::crush_mask;
use granny_core::params::{ModDest, ModSource};
use granny_core::sample::SampleBuffer;
use granny_core::scala::ScalaTuning;
use granny_core::tables::NATIVE_RATE;
use granny_core::tuning::PitchTable;
use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use nih_plug_egui::EguiState;
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

use crate::loader::{LoadOptions, StoredSample, StoredScale};
use crate::shared::{Shared, MAX_VOICES};

/// A parameter's text-to-value callback, as nih-plug wants it.
type StringToInt = Arc<dyn Fn(&str) -> Option<i32> + Send + Sync>;

/// The scale steps the editor offers, smallest first. A plugin window cannot resize itself, so the
/// editor draws itself larger inside whatever window the host gives it and the corner does the rest.
pub const UI_SCALES: [f32; 6] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.5];

/// Polyphonic modulation ids for the eight knobs, in [`granny_core::params::KNOB_RANGES`] order.
pub const POLY_MOD_RATE: u32 = 0;
pub const POLY_MOD_CRUSH: u32 = 1;
pub const POLY_MOD_ATTACK: u32 = 2;
pub const POLY_MOD_RELEASE: u32 = 3;
pub const POLY_MOD_GRAIN: u32 = 4;
pub const POLY_MOD_SHIFT: u32 = 5;
pub const POLY_MOD_START: u32 = 6;
pub const POLY_MOD_END: u32 = 7;

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum NoteMode {
    /// The firmware's TUNED bit set: notes set the playback rate.
    #[id = "pitch"]
    #[name = "pitch"]
    Pitch,
    /// TUNED clear: notes select one of 60 slices and the Rate knob sets the pitch.
    #[id = "slice"]
    #[name = "slice"]
    Slice,
}

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum PitchTableParam {
    #[id = "hardware"]
    #[name = "hardware table"]
    Hardware,
    #[id = "et"]
    #[name = "equal temperament"]
    EqualTemperament,
}

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum SnapMode {
    #[id = "off"]
    #[name = "off"]
    Off,
    #[id = "edo24"]
    #[name = "24-edo (quarter tones)"]
    Edo24,
    #[id = "edo12"]
    #[name = "12-edo (semitones)"]
    Edo12,
}

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum CurveModeParam {
    /// Reproduces the AVR's 16-bit overflow, so the shift curve folds back at the extremes.
    #[id = "hardware"]
    #[name = "hardware (16-bit, folds)"]
    Hardware,
    #[id = "extended"]
    #[name = "extended (32-bit, smooth)"]
    Extended,
}

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum MpeZoneParam {
    /// Plain MIDI: bend, pressure and CC74 apply to every voice.
    #[id = "off"]
    #[name = "off (global)"]
    Off,
    #[id = "lower"]
    #[name = "lower zone (ch 2-16)"]
    Lower,
    #[id = "upper"]
    #[name = "upper zone (ch 15-1)"]
    Upper,
}

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum ModSourceParam {
    #[id = "none"]
    #[name = "none"]
    None,
    #[id = "pressure"]
    #[name = "pressure"]
    Pressure,
    #[id = "slide"]
    #[name = "slide"]
    Slide,
    #[id = "velocity"]
    #[name = "velocity"]
    Velocity,
    #[id = "bend"]
    #[name = "bend"]
    Bend,
    #[id = "random"]
    #[name = "random"]
    Random,
}

impl ModSourceParam {
    pub fn to_core(self) -> ModSource {
        match self {
            ModSourceParam::None => ModSource::None,
            ModSourceParam::Pressure => ModSource::Pressure,
            ModSourceParam::Slide => ModSource::Slide,
            ModSourceParam::Velocity => ModSource::Velocity,
            ModSourceParam::Bend => ModSource::Bend,
            ModSourceParam::Random => ModSource::Random,
        }
    }
}

#[derive(Enum, Debug, PartialEq, Eq, Clone, Copy)]
pub enum ModDestParam {
    #[id = "none"]
    #[name = "none"]
    None,
    #[id = "rate"]
    #[name = "rate"]
    Rate,
    #[id = "crush"]
    #[name = "crush"]
    Crush,
    #[id = "attack"]
    #[name = "attack"]
    Attack,
    #[id = "release"]
    #[name = "release"]
    Release,
    #[id = "grain"]
    #[name = "grain size"]
    Grain,
    #[id = "shift"]
    #[name = "shift"]
    Shift,
    #[id = "start"]
    #[name = "start"]
    Start,
    #[id = "end"]
    #[name = "end"]
    End,
    #[id = "level"]
    #[name = "level"]
    Level,
    #[id = "pan"]
    #[name = "pan"]
    Pan,
}

impl ModDestParam {
    pub fn to_core(self) -> ModDest {
        match self {
            ModDestParam::None => ModDest::None,
            ModDestParam::Rate => ModDest::Rate,
            ModDestParam::Crush => ModDest::Crush,
            ModDestParam::Attack => ModDest::Attack,
            ModDestParam::Release => ModDest::Release,
            ModDestParam::Grain => ModDest::Grain,
            ModDestParam::Shift => ModDest::Shift,
            ModDestParam::Start => ModDest::Start,
            ModDestParam::End => ModDest::End,
            ModDestParam::Level => ModDest::Level,
            ModDestParam::Pan => ModDest::Pan,
        }
    }
}

/// One row of the modulation matrix.
#[derive(Params)]
pub struct ModRowParams {
    #[id = "msrc"]
    pub source: EnumParam<ModSourceParam>,
    #[id = "mdst"]
    pub dest: EnumParam<ModDestParam>,
    #[id = "mamt"]
    pub depth: FloatParam,
}

impl ModRowParams {
    fn new(source: ModSourceParam, dest: ModDestParam, depth: f32) -> Self {
        Self {
            source: EnumParam::new("source", source),
            dest: EnumParam::new("destination", dest),
            depth: FloatParam::new(
                "depth",
                depth,
                FloatRange::Linear {
                    min: -1.0,
                    max: 1.0,
                },
            )
            .with_step_size(0.01),
        }
    }
}

/// The sample, held in the plugin's state so an instance is a self-contained preset.
///
/// Implementing [`PersistentField`] by hand rather than storing a plain `Mutex` gives us the hook
/// we need: when a host restores state, `set` fires and we can rebuild the audio buffer and hand it
/// to the audio thread straight away.
pub struct SampleSlot {
    stored: Mutex<StoredSample>,
    shared: Arc<Shared>,
}

impl SampleSlot {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            stored: Mutex::new(StoredSample::default()),
            shared,
        }
    }

    /// Store a freshly loaded sample and publish it. Main thread only.
    pub fn store(&self, sample: StoredSample) {
        let buffer = Arc::new(sample.to_buffer());
        *self.stored.lock() = sample;
        self.shared.publish_sample(buffer);
    }

    pub fn path(&self) -> String {
        self.stored.lock().path.clone()
    }
}

impl<'a> PersistentField<'a, StoredSample> for SampleSlot {
    fn set(&self, new_value: StoredSample) {
        let buffer = Arc::new(new_value.to_buffer());
        let summary = if new_value.is_empty() {
            "no sample loaded".to_string()
        } else {
            format!(
                "restored {} ({} bytes)",
                new_value.name,
                new_value.data.len()
            )
        };
        *self.stored.lock() = new_value;
        self.shared.publish_sample(buffer);
        self.shared.set_status(summary);
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&StoredSample) -> R,
    {
        f(&self.stored.lock())
    }
}

/// The Scala scale and keyboard map, stored as their original text.
pub struct ScaleSlot {
    stored: Mutex<StoredScale>,
    shared: Arc<Shared>,
}

impl ScaleSlot {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            stored: Mutex::new(StoredScale::default()),
            shared,
        }
    }

    /// Replace the stored scale and publish the parsed result. Returns a message for the editor.
    pub fn store(&self, scale: StoredScale) -> Result<(), String> {
        let parsed = scale.to_tuning()?;
        *self.stored.lock() = scale;
        self.shared.publish_scala(parsed.map(Arc::new));
        Ok(())
    }

    pub fn update(&self, edit: impl FnOnce(&mut StoredScale)) -> Result<(), String> {
        let mut scale = self.stored.lock().clone();
        edit(&mut scale);
        self.store(scale)
    }

    pub fn snapshot(&self) -> StoredScale {
        self.stored.lock().clone()
    }
}

impl<'a> PersistentField<'a, StoredScale> for ScaleSlot {
    fn set(&self, new_value: StoredScale) {
        match new_value.to_tuning() {
            Ok(parsed) => {
                self.shared.publish_scala(parsed.map(Arc::new));
                *self.stored.lock() = new_value;
            }
            Err(err) => self
                .shared
                .set_status(format!("stored scale is invalid: {err}")),
        }
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&StoredScale) -> R,
    {
        f(&self.stored.lock())
    }
}

#[derive(Params)]
pub struct MaterParams {
    #[persist = "editor-state"]
    pub editor_state: Arc<EguiState>,
    /// How much larger than its natural size the editor draws itself. A view setting rather than a
    /// parameter — nothing automates how big the text is — but it is saved with the instance.
    #[persist = "uiscale"]
    pub ui_scale: RwLock<f32>,
    #[persist = "sample"]
    pub sample: SampleSlot,
    #[persist = "scale"]
    pub scale: ScaleSlot,

    // --- the eight knobs ---
    #[id = "rate"]
    pub rate: IntParam,
    #[id = "crush"]
    pub crush: IntParam,
    #[id = "attack"]
    pub attack: IntParam,
    #[id = "release"]
    pub release: IntParam,
    #[id = "grain"]
    pub grain: IntParam,
    #[id = "shift"]
    pub shift: IntParam,
    #[id = "start"]
    pub start: IntParam,
    #[id = "end"]
    pub end: IntParam,

    // --- the setting bits ---
    #[id = "notemode"]
    pub note_mode: EnumParam<NoteMode>,
    #[id = "legato"]
    pub legato: BoolParam,
    #[id = "repeat"]
    pub repeat: BoolParam,
    #[id = "sync"]
    pub sync: BoolParam,
    #[id = "randshift"]
    pub random_shift: BoolParam,

    // --- converted from hardware UI gestures ---
    #[id = "hold"]
    pub hold: BoolParam,
    #[id = "level"]
    pub level: FloatParam,

    // --- polyphony, tuning and expression: no hardware equivalent ---
    #[id = "voices"]
    pub voices: IntParam,
    #[id = "pitchtbl"]
    pub pitch_table: EnumParam<PitchTableParam>,
    #[id = "snap"]
    pub snap: EnumParam<SnapMode>,
    #[id = "usescala"]
    pub use_scala: BoolParam,
    #[id = "matchpitch"]
    pub match_input_pitch: BoolParam,
    #[id = "rootadj"]
    pub root_adjust: FloatParam,
    #[id = "mpezone"]
    pub mpe_zone: EnumParam<MpeZoneParam>,
    #[id = "bendrng"]
    pub bend_range: IntParam,
    #[id = "mbendrng"]
    pub master_bend_range: IntParam,
    #[id = "bendrpn"]
    pub follow_rpn: BoolParam,

    // --- fidelity switches, all defaulting to the hardware's behaviour ---
    #[id = "curve"]
    pub curve_mode: EnumParam<CurveModeParam>,
    #[id = "interp"]
    pub interpolate: BoolParam,
    #[id = "seekq"]
    pub quantize_seeks: BoolParam,
    #[id = "fade"]
    pub grain_fade: FloatParam,

    // --- load-time conditioning ---
    #[id = "resample"]
    pub resample_on_load: BoolParam,
    #[id = "normlz"]
    pub normalize_on_load: BoolParam,
    #[id = "hwcc"]
    pub hardware_cc: BoolParam,

    #[nested(array, group = "mod matrix")]
    pub mods: [ModRowParams; granny_core::params::MOD_SLOTS],
}

impl MaterParams {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            editor_state: EguiState::from_size(960, 700),
            ui_scale: RwLock::new(UI_SCALES[0]),
            sample: SampleSlot::new(shared.clone()),
            scale: ScaleSlot::new(shared),

            rate: IntParam::new("rate", 877, IntRange::Linear { min: 0, max: 1023 })
                .with_poly_modulation_id(POLY_MOD_RATE)
                .with_value_to_string(Arc::new(|v| {
                    let hz = granny_core::curves::value_to_sample_rate(v as u16, false);
                    let semitones = 12.0 * (hz as f32 / NATIVE_RATE).log2();
                    format!("{v} ({hz} hz, {semitones:+.2} st)")
                }))
                .with_string_to_value(parse_leading_int()),
            crush: IntParam::new("crush", 0, IntRange::Linear { min: 0, max: 127 })
                .with_poly_modulation_id(POLY_MOD_CRUSH)
                .with_value_to_string(Arc::new(|v| {
                    let mask = crush_mask(v as u16);
                    // Every bit the mask sets is a bit of resolution the DAC word loses.
                    let clean_bits = 12 - (16 - mask.leading_zeros()).min(12);
                    format!("{v} (mask {mask}, {clean_bits} bit)")
                }))
                .with_string_to_value(parse_leading_int()),
            attack: IntParam::new("attack", 0, IntRange::Linear { min: 0, max: 127 })
                .with_poly_modulation_id(POLY_MOD_ATTACK)
                .with_value_to_string(Arc::new(|v| format!("{v} ms/step ({} ms)", envelope_ms(v))))
                .with_string_to_value(parse_leading_int()),
            release: IntParam::new("release", 0, IntRange::Linear { min: 0, max: 127 })
                .with_poly_modulation_id(POLY_MOD_RELEASE)
                .with_value_to_string(Arc::new(|v| format!("{v} ms/step ({} ms)", envelope_ms(v))))
                .with_string_to_value(parse_leading_int()),
            grain: IntParam::new("grain size", 0, IntRange::Linear { min: 0, max: 127 })
                .with_poly_modulation_id(POLY_MOD_GRAIN)
                .with_value_to_string(Arc::new(|v| {
                    if v == 0 {
                        "0 (off)".to_string()
                    } else {
                        format!(
                            "{v} ({} ms, {} ticks)",
                            grain_ms(v as u16, CurveMode::HardwareExact),
                            sync_grain_ticks(v as u16)
                        )
                    }
                }))
                .with_string_to_value(parse_leading_int()),
            shift: IntParam::new("shift", 128, IntRange::Linear { min: 0, max: 255 })
                .with_poly_modulation_id(POLY_MOD_SHIFT)
                .with_value_to_string(Arc::new(|v| {
                    format!(
                        "{v} ({:+} b/grain)",
                        shift_bytes(v as u16, CurveMode::HardwareExact)
                    )
                }))
                .with_string_to_value(parse_leading_int()),
            start: IntParam::new("start", 0, IntRange::Linear { min: 0, max: 1023 })
                .with_poly_modulation_id(POLY_MOD_START)
                .with_value_to_string(Arc::new(|v| format!("{v} ({:.1} %)", v as f32 / 10.23)))
                .with_string_to_value(parse_leading_int()),
            end: IntParam::new("end", 1022, IntRange::Linear { min: 0, max: 1023 })
                .with_poly_modulation_id(POLY_MOD_END)
                .with_value_to_string(Arc::new(|v| {
                    if v >= 1000 {
                        format!("{v} (end of sample)")
                    } else {
                        format!(
                            "{v} ({:.1} %, {} ticks)",
                            v as f32 / 10.23,
                            sync_end_ticks(v as u16)
                        )
                    }
                }))
                .with_string_to_value(parse_leading_int()),

            note_mode: EnumParam::new("note mode", NoteMode::Pitch),
            legato: BoolParam::new("legato", false),
            repeat: BoolParam::new("repeat", true),
            sync: BoolParam::new("sync", true),
            random_shift: BoolParam::new("random shift", false),

            hold: BoolParam::new("hold", false),
            level: FloatParam::new(
                "level",
                util::db_to_gain(-6.0),
                FloatRange::Skewed {
                    min: 0.0,
                    max: util::db_to_gain(6.0),
                    factor: FloatRange::gain_skew_factor(-60.0, 6.0),
                },
            )
            // Linear, not logarithmic: the range reaches zero so silence stays reachable.
            .with_smoother(SmoothingStyle::Linear(10.0))
            .with_unit(" db")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),

            voices: IntParam::new(
                "voices",
                MAX_VOICES as i32,
                IntRange::Linear {
                    min: 1,
                    max: MAX_VOICES as i32,
                },
            ),
            // Equal temperament by default: the hardware's own table is a rounded approximation,
            // up to +10 cents sharp at the bottom, so it cannot track an incoming note exactly.
            pitch_table: EnumParam::new("pitch table", PitchTableParam::EqualTemperament),
            snap: EnumParam::new("snap", SnapMode::Off),
            use_scala: BoolParam::new("use scala scale", false),
            match_input_pitch: BoolParam::new("match input pitch", true),
            root_adjust: FloatParam::new(
                "root adjust",
                0.0,
                FloatRange::Linear {
                    min: -24.0,
                    max: 24.0,
                },
            )
            .with_step_size(0.01)
            .with_unit(" st"),
            mpe_zone: EnumParam::new("mpe", MpeZoneParam::Lower),
            // MPE's two defaults. Member channels get the wide range MPE reserves for them; the
            // master channel — and plain MIDI, with the zone off — gets the ordinary ±2, because
            // that is what an ordinary controller means by a full-scale bend.
            bend_range: IntParam::new("mpe bend range", 48, IntRange::Linear { min: 1, max: 96 })
                .with_unit(" st"),
            master_bend_range: IntParam::new(
                "midi bend range",
                2,
                IntRange::Linear { min: 1, max: 96 },
            )
            .with_unit(" st"),
            follow_rpn: BoolParam::new("follow rpn 0", true),

            curve_mode: EnumParam::new("curve maps", CurveModeParam::Hardware),
            interpolate: BoolParam::new("interpolate", false),
            quantize_seeks: BoolParam::new("block-quantise seeks", true),
            grain_fade: FloatParam::new(
                "grain fade",
                0.0,
                FloatRange::Linear { min: 0.0, max: 5.0 },
            )
            .with_step_size(0.1)
            .with_unit(" ms"),

            resample_on_load: BoolParam::new("resample on load", true),
            normalize_on_load: BoolParam::new("normalise on load", true),
            hardware_cc: BoolParam::new("hardware cc map", false),

            mods: [
                ModRowParams::new(ModSourceParam::Pressure, ModDestParam::Level, 0.6),
                ModRowParams::new(ModSourceParam::Slide, ModDestParam::Start, 1.0),
                ModRowParams::new(ModSourceParam::None, ModDestParam::None, 0.0),
            ],
        }
    }

    /// The editor's scale, held inside the offered steps in case a restored state is out of range.
    pub fn ui_scale(&self) -> f32 {
        self.ui_scale
            .read()
            .clamp(UI_SCALES[0], UI_SCALES[UI_SCALES.len() - 1])
    }

    pub fn set_ui_scale(&self, scale: f32) {
        *self.ui_scale.write() = scale;
    }

    pub fn load_options(&self) -> LoadOptions {
        LoadOptions {
            resample: self.resample_on_load.value(),
            normalize: self.normalize_on_load.value(),
        }
    }

    pub fn pitch_table(&self) -> PitchTable {
        match self.pitch_table.value() {
            PitchTableParam::Hardware => PitchTable::Hardware,
            PitchTableParam::EqualTemperament => PitchTable::EqualTemperament,
        }
    }

    pub fn snap_divisions(&self) -> Option<u32> {
        match self.snap.value() {
            SnapMode::Off => None,
            SnapMode::Edo24 => Some(24),
            SnapMode::Edo12 => Some(12),
        }
    }

    pub fn curve_mode(&self) -> CurveMode {
        match self.curve_mode.value() {
            CurveModeParam::Hardware => CurveMode::HardwareExact,
            CurveModeParam::Extended => CurveMode::Extended,
        }
    }

    /// The scale to use, if one is loaded and enabled.
    pub fn active_scala(&self, loaded: &Option<Arc<ScalaTuning>>) -> Option<Arc<ScalaTuning>> {
        if self.use_scala.value() {
            loaded.clone()
        } else {
            None
        }
    }
}

/// Parse a knob's displayed text back to its raw value.
///
/// Every knob's display starts with the hardware's own raw number and puts the derived meaning in
/// brackets after it. That keeps the round-trip exact, which matters because the hardware curves are
/// not injective: several shift values map to the same byte count, so parsing the derived figure
/// could not recover which one was meant.
fn parse_leading_int() -> StringToInt {
    Arc::new(|text| {
        let trimmed = text.trim();
        let digits: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '+')
            .collect();
        digits
            .parse::<i32>()
            .ok()
            // Fall back to a bare float so typing "50" or "50.0" also works.
            .or_else(|| trimmed.parse::<f32>().ok().map(|v| v.round() as i32))
    })
}

/// Approximate total envelope time for a step interval, matching `renderEnvelope`'s stepping.
fn envelope_ms(interval: i32) -> i32 {
    if interval == 0 {
        return 0;
    }
    // 37 indices to cross, three at a time below 30 ms.
    let steps = if interval < 30 { 13 } else { 37 };
    interval * steps
}

/// A summary of the loaded sample for the editor's header.
pub fn describe_sample(sample: &SampleBuffer) -> String {
    if sample.is_empty() {
        "no sample".to_string()
    } else {
        format!(
            "{} - {} bytes, {:.2} s at 22050 hz",
            sample.name,
            sample.len(),
            sample.len() as f32 / 22050.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_time_matches_the_step_counting() {
        assert_eq!(envelope_ms(0), 0);
        assert_eq!(envelope_ms(127), 127 * 37);
        assert_eq!(
            envelope_ms(29),
            29 * 13,
            "below 30 ms it moves three at a time"
        );
    }

    #[test]
    fn crush_display_counts_lost_bits() {
        let mask = crush_mask(127);
        assert_eq!(mask, 1016);
        let clean = 12 - (16 - mask.leading_zeros()).min(12);
        assert_eq!(clean, 2, "full crush leaves only the top two bits intact");
    }
}
