//! `set_param` / `get_param`: Schwung's stringly-typed parameter surface.
//!
//! Both run on the SPI/audio thread — `get_param` on every UI tick — so nothing in here
//! allocates. Numbers are formatted into a [`Scratch`] on the stack and everything else is
//! borrowed from the instance or from `static` text.
//!
//! Three lists have to agree, and a test holds them to it:
//!
//! - [`PARAM_KEYS`] — what goes into the state blob
//! - [`CHAIN_PARAMS`] — ranges, steps and enum options, which the Shadow UI reads
//! - `module.json`'s `ui_hierarchy` — which keys appear where, and under which knob
//!
//! `ui_hierarchy` carries no metadata, so it can never disagree with `chain_params` about a range.
//!
//! The exceptions to the no-work rule are `sample_path`, `scala_path`, `scala_kbm_path` and
//! `state`, all of which read a file or parse JSON inline. See [`crate::Instance::load_sample`].

use core::fmt::Write;

use granny_core::curves::CurveMode;
use granny_core::params::{ModDest, ModSource};
use granny_core::MpeZone;

use crate::state;
use crate::Instance;

/// A stack buffer big enough for any single formatted value.
pub struct Scratch {
    buf: [u8; 64],
    len: usize,
}

impl Scratch {
    pub fn new() -> Self {
        Self {
            buf: [0; 64],
            len: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        // Only ever written through `fmt::Write`, which hands us valid UTF-8, and only ever cut at
        // a boundary because we refuse partial writes.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for Scratch {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        if self.len + bytes.len() > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

/// Every parameter that belongs in the state blob, which is every parameter the UI can reach.
///
/// Deliberately excludes the file paths: the sample and the scale are carried by the blob itself,
/// as bytes and as text, so that a preset does not depend on where they happened to be read from.
pub const PARAM_KEYS: &[&str] = &[
    "rate",
    "crush",
    "attack",
    "release",
    "grain",
    "shift",
    "start",
    "end",
    "note_mode",
    "slice_channel",
    "level",
    "vel_sensitivity",
    "legato",
    "repeat",
    "sync",
    "random_shift",
    "hold",
    "voices",
    "pitch_table",
    "snap",
    "match_input_pitch",
    "root_adjust",
    "mpe_zone",
    "mpe_bend_range",
    "bend_range",
    "follow_rpn",
    "hardware_cc_map",
    "mod1_source",
    "mod1_dest",
    "mod1_depth",
    "mod2_source",
    "mod2_dest",
    "mod2_depth",
    "mod3_source",
    "mod3_dest",
    "mod3_depth",
    "curve_mode",
    "interpolate",
    "quantize_seeks",
    "grain_fade_ms",
];

// --- set ---------------------------------------------------------------------------------------

pub fn set(inst: &mut Instance, key: &str, val: &str) {
    // Mod slots are three of everything; matching them by name first keeps the table below flat.
    if let Some((slot, field)) = mod_slot_key(key) {
        set_mod(inst, slot, field, val);
        return;
    }

    match key {
        // The eight knobs, in the firmware's own integer ranges so a value here means the same
        // thing it means on the hardware.
        "rate" => inst.params.rate = clamp_u16(val, 0, 1023, inst.params.rate),
        "crush" => inst.params.crush = clamp_u16(val, 0, 127, inst.params.crush),
        "attack" => inst.params.attack = clamp_u16(val, 0, 127, inst.params.attack),
        "release" => inst.params.release = clamp_u16(val, 0, 127, inst.params.release),
        "grain" => inst.params.grain = clamp_u16(val, 0, 127, inst.params.grain),
        "shift" => inst.params.shift = clamp_u16(val, 0, 255, inst.params.shift),
        "start" => inst.params.start = clamp_u16(val, 0, 1023, inst.params.start),
        "end" => inst.params.end = clamp_u16(val, 0, 1023, inst.params.end),

        // The firmware's TUNED bit, plus the split the hardware cannot express because it has one
        // TUNED bit for the whole instrument.
        "note_mode" => {
            inst.note_mode_index = index(val).clamp(0, 2) as u8;
            inst.apply_note_mode();
        }
        "slice_channel" => {
            // Shown 1-based, held 0-based, as MIDI delivers it.
            inst.slice_channel = clamp_u16(val, 1, 16, inst.slice_channel as u16 + 1) as u8 - 1;
            inst.apply_note_mode();
        }

        "legato" => inst.params.legato = flag(val),
        "repeat" => inst.params.repeat = flag(val),
        "sync" => inst.params.sync = flag(val),
        "random_shift" => inst.params.random_shift = flag(val),
        "hold" => inst.params.hold = flag(val),

        "level" => inst.params.level = clamp_f32(val, 0.0, 1.0, inst.params.level),
        "vel_sensitivity" => {
            inst.params.vel_sensitivity = clamp_f32(val, 0.0, 1.0, inst.params.vel_sensitivity)
        }

        // Fidelity. Every one of these defaults to the hardware's behaviour.
        "curve_mode" => {
            inst.fidelity.curve_mode = match index(val) {
                1 => CurveMode::Extended,
                _ => CurveMode::HardwareExact,
            }
        }
        "interpolate" => inst.fidelity.interpolate = flag(val),
        "quantize_seeks" => inst.fidelity.quantize_seeks = flag(val),
        "grain_fade_ms" => {
            inst.fidelity.grain_fade_ms = clamp_f32(val, 0.0, 20.0, inst.fidelity.grain_fade_ms)
        }

        // Tuning.
        "pitch_table" => {
            inst.tuning.table = match index(val) {
                1 => granny_core::PitchTable::Hardware,
                _ => granny_core::PitchTable::EqualTemperament,
            }
        }
        "snap" => {
            inst.tuning.snap_divisions = match index(val) {
                1 => Some(24),
                2 => Some(12),
                _ => None,
            }
        }
        "match_input_pitch" => {
            inst.match_input_pitch = flag(val);
            inst.apply_root();
        }
        "root_adjust" => {
            inst.root_adjust = clamp_f32(val, -2400.0, 2400.0, inst.root_adjust);
            inst.apply_root();
        }

        // MPE. The zone is forced off inside a split — see `Instance::sync_mpe`.
        "mpe_zone" => {
            inst.mpe_zone = match index(val) {
                1 => MpeZone::Lower,
                2 => MpeZone::Upper,
                _ => MpeZone::Off,
            };
            inst.sync_mpe();
        }
        "mpe_bend_range" => {
            inst.mpe_bend_range = clamp_f32(val, 0.0, 48.0, inst.mpe_bend_range);
            inst.sync_mpe();
        }
        "bend_range" => {
            inst.bend_range = clamp_f32(val, 0.0, 48.0, inst.bend_range);
            inst.sync_mpe();
        }
        "follow_rpn" => {
            inst.follow_rpn = flag(val);
            inst.sync_mpe();
        }

        // Not a zone of its own: the synth-agnostic switch a tool sets when it wants MPE and has
        // no opinion about which zone. See `Instance::set_mpe_enabled`.
        "mpe_enabled" => inst.set_mpe_enabled(flag(val)),

        "hardware_cc_map" => inst.hardware_cc_map = flag(val),

        "voices" => {
            let voices = clamp_u16(val, 1, 16, inst.engine.capacity() as u16) as usize;
            if voices != inst.engine.capacity() {
                inst.engine.set_voice_capacity(voices);
            }
        }

        // Not realtime-safe: each of these reads a file or parses JSON inline. Phase 3.
        "sample_path" => inst.load_sample(val),
        // What the browser sends back: the index of the row that was picked.
        "sample_index" => inst.load_sample_index(index(val).max(0) as usize),
        "rescan_samples" => inst.scan_samples(),
        "scala_path" => inst.load_scala(val, false),
        "scala_kbm_path" => inst.load_scala(val, true),
        "state" => state::restore(inst, val),

        _ => {}
    }
}

fn set_mod(inst: &mut Instance, slot: usize, field: &str, val: &str) {
    let slot = &mut inst.mods[slot];
    match field {
        "source" => slot.source = mod_source(index(val)),
        "dest" => slot.dest = mod_dest(index(val)),
        "depth" => slot.depth = clamp_f32(val, -1.0, 1.0, slot.depth),
        _ => {}
    }
}

// --- get ---------------------------------------------------------------------------------------

/// Read a parameter back. Borrows from `inst` or writes into `scratch`; never allocates.
///
/// `state` is not served here — it is far too big for [`Scratch`] and is written straight into the
/// host's own buffer by [`crate::state::write`].
pub fn get<'a>(inst: &'a Instance, key: &str, scratch: &'a mut Scratch) -> Option<&'a str> {
    // Values that are already text can be handed straight back.
    match key {
        "chain_params" => return Some(CHAIN_PARAMS),
        "ui_hierarchy" => return Some(UI_HIERARCHY),
        "sample_path" => return Some(&inst.sample_path),
        "sample_name" => return Some(&inst.sample.name),
        "scala_name" => return Some(scale_name(&inst.scala_scl)),
        _ => {}
    }

    if let Some((slot, field)) = mod_slot_key(key) {
        let slot = &inst.mods[slot];
        let ok = match field {
            "source" => write!(scratch, "{}", slot.source as u8),
            "dest" => write!(scratch, "{}", slot.dest as u8),
            "depth" => write!(scratch, "{:.2}", slot.depth),
            _ => return None,
        };
        return ok.ok().map(|()| scratch.as_str());
    }

    let ok = match key {
        "rate" => write!(scratch, "{}", inst.params.rate),
        "crush" => write!(scratch, "{}", inst.params.crush),
        "attack" => write!(scratch, "{}", inst.params.attack),
        "release" => write!(scratch, "{}", inst.params.release),
        "grain" => write!(scratch, "{}", inst.params.grain),
        "shift" => write!(scratch, "{}", inst.params.shift),
        "start" => write!(scratch, "{}", inst.params.start),
        "end" => write!(scratch, "{}", inst.params.end),

        "note_mode" => write!(scratch, "{}", inst.note_mode_index),
        "slice_channel" => write!(scratch, "{}", inst.slice_channel + 1),

        "legato" => write!(scratch, "{}", inst.params.legato as u8),
        "repeat" => write!(scratch, "{}", inst.params.repeat as u8),
        "sync" => write!(scratch, "{}", inst.params.sync as u8),
        "random_shift" => write!(scratch, "{}", inst.params.random_shift as u8),
        "hold" => write!(scratch, "{}", inst.params.hold as u8),

        "level" => write!(scratch, "{:.3}", inst.params.level),
        "vel_sensitivity" => write!(scratch, "{:.3}", inst.params.vel_sensitivity),

        "curve_mode" => write!(
            scratch,
            "{}",
            match inst.fidelity.curve_mode {
                CurveMode::HardwareExact => 0,
                CurveMode::Extended => 1,
            }
        ),
        "interpolate" => write!(scratch, "{}", inst.fidelity.interpolate as u8),
        "quantize_seeks" => write!(scratch, "{}", inst.fidelity.quantize_seeks as u8),
        "grain_fade_ms" => write!(scratch, "{:.1}", inst.fidelity.grain_fade_ms),

        "pitch_table" => write!(
            scratch,
            "{}",
            match inst.tuning.table {
                granny_core::PitchTable::EqualTemperament => 0,
                granny_core::PitchTable::Hardware => 1,
            }
        ),
        "snap" => write!(
            scratch,
            "{}",
            match inst.tuning.snap_divisions {
                Some(24) => 1,
                Some(12) => 2,
                _ => 0,
            }
        ),
        "match_input_pitch" => write!(scratch, "{}", inst.match_input_pitch as u8),
        "root_adjust" => write!(scratch, "{:.0}", inst.root_adjust),

        "mpe_zone" => write!(
            scratch,
            "{}",
            match inst.mpe_zone {
                MpeZone::Off => 0,
                MpeZone::Lower => 1,
                MpeZone::Upper => 2,
            }
        ),
        "mpe_bend_range" => write!(scratch, "{:.0}", inst.mpe_bend_range),
        "bend_range" => write!(scratch, "{:.0}", inst.bend_range),
        "follow_rpn" => write!(scratch, "{}", inst.follow_rpn as u8),
        "hardware_cc_map" => write!(scratch, "{}", inst.hardware_cc_map as u8),

        "voices" => write!(scratch, "{}", inst.engine.capacity()),

        // Read-only readouts, for a UI that wants to say what is actually going on.
        //
        // `mpe_active` is the zone in force rather than the one asked for: inside a split it is
        // always Off. That is what the CLAP editor greys the MPE controls out on, and it is the
        // only way a UI can tell the difference without reimplementing the rule.
        // Whether MPE is in force at all, for a caller that set `mpe_enabled` and wants it back.
        "mpe_enabled" => write!(
            scratch,
            "{}",
            (inst.effective_mpe_zone() != MpeZone::Off) as u8
        ),
        "mpe_active" => write!(
            scratch,
            "{}",
            match inst.effective_mpe_zone() {
                MpeZone::Off => 0,
                MpeZone::Lower => 1,
                MpeZone::Upper => 2,
            }
        ),
        "sample_root" => write!(scratch, "{:.2}", inst.tuning.root_note),
        "sample_frames" => write!(scratch, "{}", inst.sample.len()),
        "active_voices" => write!(scratch, "{}", inst.engine.active_voices()),

        _ => return None,
    };

    ok.ok().map(|()| scratch.as_str())
}

// --- parsing -----------------------------------------------------------------------------------

/// Split `mod2_depth` into slot 1 and `"depth"`. Returns `None` for anything else.
fn mod_slot_key(key: &str) -> Option<(usize, &str)> {
    let rest = key.strip_prefix("mod")?;
    let (digit, field) = rest.split_at_checked(1)?;
    let slot = digit.parse::<usize>().ok()?;
    if !(1..=granny_core::MOD_SLOTS).contains(&slot) {
        return None;
    }
    Some((slot - 1, field.strip_prefix('_')?))
}

fn mod_source(index: i32) -> ModSource {
    match index {
        1 => ModSource::Pressure,
        2 => ModSource::Slide,
        3 => ModSource::Velocity,
        4 => ModSource::Bend,
        5 => ModSource::Random,
        _ => ModSource::None,
    }
}

fn mod_dest(index: i32) -> ModDest {
    match index {
        1 => ModDest::Rate,
        2 => ModDest::Crush,
        3 => ModDest::Attack,
        4 => ModDest::Release,
        5 => ModDest::Grain,
        6 => ModDest::Shift,
        7 => ModDest::Start,
        8 => ModDest::End,
        9 => ModDest::Level,
        10 => ModDest::Pan,
        _ => ModDest::None,
    }
}

/// A Scala file names itself in its first non-comment line. Cheap enough to find on demand, and it
/// saves keeping a third copy of the text around just to have something to display.
fn scale_name(scl: &str) -> &str {
    scl.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('!'))
        .unwrap_or("")
}

fn clamp_u16(val: &str, min: u16, max: u16, fallback: u16) -> u16 {
    // Accept "877" and "877.0" alike; the Shadow UI sends whichever the param type implies.
    match val.trim().parse::<f32>() {
        Ok(v) if v.is_finite() => (v.round() as i64).clamp(min as i64, max as i64) as u16,
        _ => fallback,
    }
}

fn clamp_f32(val: &str, min: f32, max: f32, fallback: f32) -> f32 {
    match val.trim().parse::<f32>() {
        Ok(v) if v.is_finite() => v.clamp(min, max),
        _ => fallback,
    }
}

/// Enum parameters arrive as their option index.
fn index(val: &str) -> i32 {
    val.trim().parse::<f32>().map_or(0, |v| v.round() as i32)
}

/// On/off arrives as an index too, but tolerate the words in case a patch file carries them.
fn flag(val: &str) -> bool {
    match val.trim() {
        "true" | "on" | "On" | "yes" => true,
        "false" | "off" | "Off" | "no" => false,
        other => index(other) != 0,
    }
}

/// The three mod slots are identical but for their prefix, and writing them out three times is
/// three chances to mistype an option list.
macro_rules! mod_slot_params {
    ($prefix:literal, $label:literal) => {
        concat!(
            r#"{"key":""#, $prefix, r#"_source","name":""#, $label, r#" Src","type":"enum","#,
            r#""options":["None","Pressure","Slide","Velocity","Bend","Random"],"default":0},"#,
            r#"{"key":""#, $prefix, r#"_dest","name":""#, $label, r#" Dest","type":"enum","#,
            r#""options":["None","Rate","Crush","Attack","Release","Grain","Shift","Start","End","Level","Pan"],"default":0},"#,
            r#"{"key":""#, $prefix, r#"_depth","name":""#, $label, r#" Depth","type":"float","min":-1,"max":1,"step":0.05,"default":0},"#,
        )
    };
}

/// The menu structure, which a chainable sound generator has to serve itself.
///
/// The chain host falls back to `module.json` for an audio FX's hierarchy but not for a
/// synth's — `chain_host.c` forwards `synth:ui_hierarchy` straight to the plugin. A module that
/// only puts the hierarchy in `module.json`, as this one did, loads into a slot with no menu at
/// all: every parameter exists and none of them can be reached.
///
/// `module.json` keeps its copy for the module manager and the standalone host. A test holds
/// the two to being the same JSON.
const UI_HIERARCHY: &str = r#"{"levels":{"root":{"label":"mater","knobs":["rate","crush","attack","release","grain","shift","start","end"],"params":[{"level":"sample","label":"Sample..."},{"key":"rate","label":"Rate"},{"key":"crush","label":"Crush"},{"key":"attack","label":"Attack"},{"key":"release","label":"Release"},{"key":"grain","label":"Grain Size"},{"key":"shift","label":"Shift"},{"key":"start","label":"Start"},{"key":"end","label":"End"},{"key":"note_mode","label":"Note Mode"},{"key":"slice_channel","label":"Slice Chan"},{"key":"level","label":"Level"},{"level":"settings","label":"Settings"},{"level":"tuning","label":"Tuning"},{"level":"mpe","label":"MPE"},{"level":"mod_matrix","label":"Mod Matrix"},{"level":"fidelity","label":"Fidelity"}]},"settings":{"label":"Settings","knobs":["legato","repeat","sync","random_shift"],"params":[{"key":"legato","label":"Legato"},{"key":"repeat","label":"Repeat"},{"key":"sync","label":"Sync"},{"key":"random_shift","label":"Random Shift"},{"key":"hold","label":"Hold"},{"key":"vel_sensitivity","label":"Vel Sens"},{"key":"voices","label":"Voices"},{"key":"hardware_cc_map","label":"HW CC Map"}]},"tuning":{"label":"Tuning","knobs":["match_input_pitch","root_adjust","pitch_table","snap"],"params":[{"key":"match_input_pitch","label":"Match Pitch"},{"key":"root_adjust","label":"Root Adjust"},{"key":"pitch_table","label":"Pitch Table"},{"key":"snap","label":"Snap"}]},"mpe":{"label":"MPE","knobs":["mpe_zone","mpe_bend_range","bend_range","follow_rpn"],"params":[{"key":"mpe_zone","label":"Zone"},{"key":"mpe_bend_range","label":"MPE Bend"},{"key":"bend_range","label":"MIDI Bend"},{"key":"follow_rpn","label":"Follow RPN 0"}]},"mod_matrix":{"label":"Mod Matrix","knobs":["mod1_source","mod1_dest","mod1_depth","mod2_depth"],"params":[{"key":"mod1_source","label":"1 Source"},{"key":"mod1_dest","label":"1 Dest"},{"key":"mod1_depth","label":"1 Depth"},{"key":"mod2_source","label":"2 Source"},{"key":"mod2_dest","label":"2 Dest"},{"key":"mod2_depth","label":"2 Depth"},{"key":"mod3_source","label":"3 Source"},{"key":"mod3_dest","label":"3 Dest"},{"key":"mod3_depth","label":"3 Depth"}]},"fidelity":{"label":"Fidelity","knobs":["curve_mode","interpolate","quantize_seeks","grain_fade_ms"],"params":[{"key":"curve_mode","label":"Curve Maps"},{"key":"interpolate","label":"Interpolate"},{"key":"quantize_seeks","label":"Block Seeks"},{"key":"grain_fade_ms","label":"Grain Fade"}]},"sample":{"label":"Sample","items_param":"sample_list","select_param":"sample_index","navigate_to":"root"}}}"#;

/// Ranges, steps and enum options for the Shadow UI. Order does not matter here — `ui_hierarchy`
/// in `module.json` decides what appears where.
const CHAIN_PARAMS: &str = concat!(
    "[",
    // Float, not int, and that is the whole point: `knob_engine.mjs` only reads `step` on the
    // float path. Its int path accumulates detents and emits one unit once the accumulator
    // reaches the acceleration divisor, so an int crosses this range in 4089 detents at a fast
    // sweep no matter what step it declares. 32 puts the sweep at 125 and still leaves a slow
    // turn moving 2 units — just under the 2.4 that `value_to_sample_rate` needs to change the
    // DAC rate at all, so nothing below the curve's own resolution is lost. `display_format`
    // keeps the row reading "877" rather than "877.00"; `set_param` already took floats.
    r#"{"key":"rate","name":"Rate","type":"float","min":0,"max":1023,"step":32,"display_format":".0f","default":877},"#,
    r#"{"key":"crush","name":"Crush","type":"int","min":0,"max":127,"default":0},"#,
    r#"{"key":"attack","name":"Attack","type":"int","min":0,"max":127,"default":0},"#,
    r#"{"key":"release","name":"Release","type":"int","min":0,"max":127,"default":0},"#,
    r#"{"key":"grain","name":"Grain Size","type":"int","min":0,"max":127,"default":0},"#,
    r#"{"key":"shift","name":"Shift","type":"int","min":0,"max":255,"default":128},"#,
    // Start and end are the same sweep and take the same treatment. Every unit here is a real
    // granule of the file rather than a rounding error, so the slow turn's 2 units stay useful,
    // and the firmware refuses a loop shorter than ten granules anyway.
    r#"{"key":"start","name":"Start","type":"float","min":0,"max":1023,"step":32,"display_format":".0f","default":0},"#,
    r#"{"key":"end","name":"End","type":"float","min":0,"max":1023,"step":32,"display_format":".0f","default":1022},"#,
    r#"{"key":"note_mode","name":"Note Mode","type":"enum","options":["Pitch","Slice","Split"],"default":0},"#,
    r#"{"key":"slice_channel","name":"Slice Chan","type":"int","min":1,"max":16,"default":1},"#,
    r#"{"key":"level","name":"Level","type":"float","min":0,"max":1,"step":0.02,"default":0.5},"#,
    r#"{"key":"vel_sensitivity","name":"Vel Sens","type":"float","min":0,"max":1,"step":0.05,"default":0.4},"#,
    r#"{"key":"legato","name":"Legato","type":"enum","options":["Off","On"],"default":0},"#,
    r#"{"key":"repeat","name":"Repeat","type":"enum","options":["Off","On"],"default":1},"#,
    r#"{"key":"sync","name":"Sync","type":"enum","options":["Off","On"],"default":1},"#,
    r#"{"key":"random_shift","name":"Random Shift","type":"enum","options":["Off","On"],"default":0},"#,
    r#"{"key":"hold","name":"Hold","type":"enum","options":["Off","On"],"default":0},"#,
    r#"{"key":"voices","name":"Voices","type":"int","min":1,"max":16,"default":8},"#,
    r#"{"key":"pitch_table","name":"Pitch Table","type":"enum","options":["Equal","Hardware"],"default":0},"#,
    r#"{"key":"snap","name":"Snap","type":"enum","options":["Off","24-EDO","12-EDO"],"default":0},"#,
    r#"{"key":"match_input_pitch","name":"Match Pitch","type":"enum","options":["Off","On"],"default":1},"#,
    r#"{"key":"root_adjust","name":"Root Adjust","type":"int","min":-2400,"max":2400,"unit":"c","default":0},"#,
    r#"{"key":"mpe_zone","name":"MPE Zone","type":"enum","options":["Off","Lower","Upper"],"default":0},"#,
    r#"{"key":"mpe_bend_range","name":"MPE Bend","type":"int","min":0,"max":48,"unit":"st","default":48},"#,
    r#"{"key":"bend_range","name":"MIDI Bend","type":"int","min":0,"max":48,"unit":"st","default":2},"#,
    r#"{"key":"follow_rpn","name":"Follow RPN 0","type":"enum","options":["Off","On"],"default":1},"#,
    r#"{"key":"hardware_cc_map","name":"HW CC Map","type":"enum","options":["Off","On"],"default":0},"#,
    mod_slot_params!("mod1", "Mod 1"),
    mod_slot_params!("mod2", "Mod 2"),
    mod_slot_params!("mod3", "Mod 3"),
    r#"{"key":"curve_mode","name":"Curve Maps","type":"enum","options":["Hardware","Extended"],"default":0},"#,
    r#"{"key":"interpolate","name":"Interpolate","type":"enum","options":["Off","On"],"default":0},"#,
    r#"{"key":"quantize_seeks","name":"Block Seeks","type":"enum","options":["Off","On"],"default":1},"#,
    r#"{"key":"grain_fade_ms","name":"Grain Fade","type":"float","min":0,"max":20,"step":0.5,"unit":"ms","default":0}"#,
    "]"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key `chain_params` advertises, in the order it advertises them.
    fn advertised_keys() -> Vec<&'static str> {
        CHAIN_PARAMS
            .split("\"key\":\"")
            .skip(1)
            .map(|rest| &rest[..rest.find('"').unwrap()])
            .collect()
    }

    #[test]
    fn the_state_blob_and_the_ui_cover_the_same_parameters() {
        let advertised = advertised_keys();
        for key in PARAM_KEYS {
            assert!(
                advertised.contains(key),
                "{key} is persisted but the UI cannot reach it"
            );
        }
        for key in &advertised {
            assert!(
                PARAM_KEYS.contains(key),
                "{key} is in the UI but would not survive a reload"
            );
        }
    }

    /// The third list, which lives outside the crate: `module.json` decides what the Shadow UI
    /// draws, and a key that only exists in two of the three places is a row that never appears or
    /// a row that reads blank.
    #[test]
    fn module_json_lays_out_exactly_the_parameters_that_exist() {
        const MODULE_JSON: &str = include_str!("../../../schwung/module.json");
        let module: serde_json::Value = serde_json::from_str(MODULE_JSON).unwrap();
        let levels = module["capabilities"]["ui_hierarchy"]["levels"]
            .as_object()
            .expect("ui_hierarchy.levels");

        let mut laid_out = Vec::new();
        for (name, level) in levels {
            // A selection level (`items_param`) has no params of its own — its rows come from the
            // plugin at runtime. Its `select_param` is a command, not a stored parameter.
            if level.get("items_param").is_some() {
                assert!(
                    level.get("select_param").is_some(),
                    "{name} offers items with no way to pick one"
                );
                continue;
            }
            for param in level["params"].as_array().expect("params").iter() {
                if let Some(key) = param.get("key").and_then(|k| k.as_str()) {
                    laid_out.push(key);
                } else if let Some(target) = param.get("level").and_then(|l| l.as_str()) {
                    assert!(
                        levels.contains_key(target),
                        "{name} points at a missing level"
                    );
                }
            }
            // A knob has to be bound to something the same level actually lists.
            for knob in level["knobs"].as_array().into_iter().flatten() {
                let knob = knob.as_str().unwrap();
                assert!(
                    level["params"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|p| p.get("key").and_then(|k| k.as_str()) == Some(knob)),
                    "{name} maps a knob to {knob}, which it does not list"
                );
            }
        }

        for key in PARAM_KEYS {
            assert!(
                laid_out.contains(key),
                "{key} exists but the UI never shows it"
            );
        }
        for key in &laid_out {
            assert!(
                PARAM_KEYS.contains(key),
                "module.json shows {key}, which does not exist"
            );
        }
    }

    /// The loader caps `module.json` at 8 KB and parses it with a minimal reader.
    #[test]
    fn module_json_stays_within_the_loader_limit() {
        const MODULE_JSON: &str = include_str!("../../../schwung/module.json");
        assert!(
            MODULE_JSON.len() < 8192,
            "module.json is {} bytes, over the loader's 8 KB cap",
            MODULE_JSON.len()
        );
    }

    #[test]
    fn chain_params_is_well_formed_json() {
        let parsed: serde_json::Value = serde_json::from_str(CHAIN_PARAMS).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), PARAM_KEYS.len());
    }

    #[test]
    fn mod_slot_keys_split_into_a_slot_and_a_field() {
        assert_eq!(mod_slot_key("mod1_source"), Some((0, "source")));
        assert_eq!(mod_slot_key("mod3_depth"), Some((2, "depth")));
        assert_eq!(mod_slot_key("mod4_depth"), None, "there are only three");
        assert_eq!(mod_slot_key("mod0_depth"), None, "slots are 1-based");
        assert_eq!(mod_slot_key("mode"), None, "must not swallow other keys");
        assert_eq!(mod_slot_key("modulate"), None);
    }

    #[test]
    fn scratch_refuses_to_overflow() {
        let mut scratch = Scratch::new();
        assert!(write!(scratch, "{}", "x".repeat(100)).is_err());
    }

    #[test]
    fn knob_values_clamp_to_the_hardware_range() {
        assert_eq!(clamp_u16("2000", 0, 1023, 0), 1023);
        assert_eq!(clamp_u16("-5", 0, 1023, 0), 0);
        assert_eq!(clamp_u16("877.0", 0, 1023, 0), 877);
    }

    #[test]
    fn a_value_that_will_not_parse_leaves_the_parameter_alone() {
        assert_eq!(clamp_u16("banana", 0, 1023, 877), 877);
        assert_eq!(clamp_f32("", 0.0, 1.0, 0.5), 0.5);
    }

    #[test]
    fn flags_take_an_index_or_a_word() {
        assert!(flag("1"));
        assert!(flag("on"));
        assert!(!flag("0"));
        assert!(!flag("off"));
    }

    #[test]
    fn a_scale_is_named_by_its_first_line_that_is_not_a_comment() {
        assert_eq!(
            scale_name("! meantone.scl\n!\nQuarter-comma\n 12\n"),
            "Quarter-comma"
        );
        assert_eq!(scale_name(""), "");
    }
}

#[cfg(test)]
mod hierarchy_tests {
    use super::*;

    /// The hierarchy exists twice: here, where the chain host reads it, and in `module.json`,
    /// where the module manager and the standalone host read it. They have to be the same.
    #[test]
    fn the_two_copies_of_the_hierarchy_agree() {
        const MODULE_JSON: &str = include_str!("../../../schwung/module.json");
        let module: serde_json::Value = serde_json::from_str(MODULE_JSON).unwrap();
        let from_manifest = &module["capabilities"]["ui_hierarchy"];
        let served: serde_json::Value = serde_json::from_str(UI_HIERARCHY).unwrap();
        assert_eq!(
            &served, from_manifest,
            "module.json and UI_HIERARCHY have drifted apart"
        );
    }

    /// The chain host rejects a synth whose hierarchy or params overrun the shadow param buffer,
    /// and loads it with a NULL instance so the UI can show the error.
    #[test]
    fn what_is_served_fits_the_hosts_buffer() {
        const SHADOW_PARAM_VALUE_LEN: usize = 65536;
        assert!(UI_HIERARCHY.len() < SHADOW_PARAM_VALUE_LEN - 1);
        assert!(CHAIN_PARAMS.len() < SHADOW_PARAM_VALUE_LEN - 1);
    }
}
