//! Move's MIDI bytes into engine events.
//!
//! Schwung hands over plain 3-byte channel messages, already filtered and velocity-curved by the
//! host unless the module asks for `raw_midi`. That is a much smaller surface than CLAP's typed
//! `NoteEvent`s: there are no voice ids, so every note uses [`fallback_voice_id`] the way the CLAP
//! build does when a host declines to supply one, and there is no per-note expression, so bend,
//! pressure and slide arrive per channel and are resolved through [`MpeState`] — the same state
//! machine the CLAP build uses for its own MIDI input, now living in `granny-core`.

use granny_core::params::{KNOBS, KNOB_RANGES};
use granny_core::{fallback_voice_id, Expression, Scene, Target, VoiceKey};

use crate::Instance;

/// The first CC of the hardware's parameter map. CC102..=109 are the eight knobs, in order.
const HARDWARE_CC_BASE: u8 = 102;

const CC_MODWHEEL: u8 = 1;
const CC_SUSTAIN: u8 = 64;
const CC_SLIDE: u8 = 74;
const CC_ALL_SOUND_OFF: u8 = 120;
const CC_ALL_NOTES_OFF: u8 = 123;

pub fn handle(inst: &mut Instance, msg: &[u8]) {
    let Some(&status) = msg.first() else { return };

    // Realtime and system-common messages carry no channel; the transport reaches us through the
    // host's tempo and beat-position calls instead.
    if !(0x80..0xF0).contains(&status) {
        return;
    }

    let channel = status & 0x0F;
    let data1 = msg.get(1).copied().unwrap_or(0) & 0x7F;
    let data2 = msg.get(2).copied().unwrap_or(0) & 0x7F;

    inst.sync_mpe();

    match status & 0xF0 {
        // Note on with velocity 0 is a note off, as it has been since running status.
        0x90 if data2 > 0 => note_on(inst, channel, data1, data2 as f32 / 127.0),
        0x80 | 0x90 => inst.engine.note_off(None, Some(channel), Some(data1)),

        // Polyphonic aftertouch names its note, so it reaches one voice and bypasses the channel
        // model entirely — there is nothing for MPE to resolve.
        0xA0 => {
            let pressure = data2 as f32 / 127.0;
            inst.engine
                .for_each_matching(None, Some(channel), Some(data1), |expr| {
                    expr.pressure = pressure
                });
        }

        0xB0 => control_change(inst, channel, data1, data2),

        0xD0 => {
            let target = inst.mpe.set_pressure(channel, data1 as f32 / 127.0);
            refresh(inst, target);
        }

        0xE0 => {
            // 14-bit, centred on 8192. Mapped so the centre is exactly 0.5, which is the rest
            // position `MpeState` expects.
            let raw = ((data2 as i32) << 7) | data1 as i32;
            let normalized = 0.5 + (raw - 8192) as f32 / 16384.0;
            let target = inst.mpe.set_bend(channel, normalized);
            refresh(inst, target);
        }

        _ => {}
    }
}

fn note_on(inst: &mut Instance, channel: u8, note: u8, velocity: f32) {
    // Starting a voice against an empty buffer gives it a zero-length file to read.
    if inst.sample.is_empty() {
        return;
    }

    let key = VoiceKey {
        voice_id: fallback_voice_id(channel, note),
        channel,
        note,
    };

    // A new voice inherits whatever its channel is already holding, which is what makes a note
    // struck on a controller mid-bend start bent rather than snapping there afterwards.
    let expr = Expression {
        velocity,
        bend: inst.mpe.bend_for(channel),
        bend_semitones: inst.mpe.bend_semitones_for(channel),
        pressure: inst.mpe.pressure_for(channel),
        slide: inst.mpe.slide_for(channel),
        ..Expression::default()
    };

    let transport = inst.transport_snapshot();

    // Built field by field rather than through a helper: the scene borrows several fields of
    // `inst` while `engine` is borrowed mutably, which only type-checks as disjoint places.
    let scene = Scene {
        sample: &inst.sample,
        params: &inst.params,
        fidelity: &inst.fidelity,
        tuning: &inst.tuning,
        mods: &inst.mods,
        transport,
    };

    // The return value names a voice that had to be stolen. A CLAP host wants telling; Schwung has
    // nowhere to put it.
    let _stolen = inst.engine.note_on(&scene, key, velocity, expr);
}

/// Push the channel model's current values onto whichever voices the message reached.
///
/// All four expressions are rewritten rather than just the one that changed, because `MpeState` is
/// the authority and a member channel's resolved value depends on the master channel too.
fn refresh(inst: &mut Instance, target: Target) {
    let mpe = &inst.mpe;
    match target {
        Target::All => inst.engine.for_each_all(|channel, expr| {
            expr.bend = mpe.bend_for(channel);
            expr.bend_semitones = mpe.bend_semitones_for(channel);
            expr.pressure = mpe.pressure_for(channel);
            expr.slide = mpe.slide_for(channel);
        }),
        Target::Channel(channel) => {
            let (bend, semitones) = (mpe.bend_for(channel), mpe.bend_semitones_for(channel));
            let (pressure, slide) = (mpe.pressure_for(channel), mpe.slide_for(channel));
            inst.engine
                .for_each_matching(None, Some(channel), None, |expr| {
                    expr.bend = bend;
                    expr.bend_semitones = semitones;
                    expr.pressure = pressure;
                    expr.slide = slide;
                });
        }
    }
}

fn control_change(inst: &mut Instance, channel: u8, controller: u8, value: u8) {
    // RPN 0 is how a controller announces its own bend range. Handled before anything else so the
    // data-entry CCs are not also read as something on the hardware map.
    if inst.follow_rpn
        && inst
            .mpe
            .handle_rpn(channel, controller, value as f32 / 127.0)
            .is_some()
    {
        refresh(inst, Target::All);
        return;
    }

    match controller {
        // Sustain is global here, as it is in the CLAP build: the engine holds every voice.
        CC_SUSTAIN => inst.engine.set_sustain(value >= 64),

        CC_SLIDE => {
            let target = inst.mpe.set_slide(channel, value as f32 / 127.0);
            refresh(inst, target);
        }

        CC_ALL_SOUND_OFF => inst.engine.choke(None, None, None),
        CC_ALL_NOTES_OFF => inst.engine.all_notes_off(),

        _ if inst.hardware_cc_map => hardware_cc(inst, controller, value),
        _ => {}
    }
}

/// The microGranny's own CC map, so an existing rig drives the module unchanged.
///
/// The CLAP build has to keep these in a side table of overrides, because a plugin cannot write to
/// its own parameters from the audio thread and the displayed knob would lie. Schwung has no such
/// split — `set_param` is the only writer and the Shadow UI reads the same struct back — so here
/// they go straight into the parameters and the display follows.
fn hardware_cc(inst: &mut Instance, controller: u8, value: u8) {
    let index = match controller {
        // Modwheel is wired to crush on the 2.5.
        CC_MODWHEEL => 1,
        cc if (HARDWARE_CC_BASE..HARDWARE_CC_BASE + KNOBS as u8).contains(&cc) => {
            (cc - HARDWARE_CC_BASE) as usize
        }
        _ => return,
    };

    // The firmware scales the 7-bit CC up to each parameter's own bit depth.
    let scaled = (value as f32 * KNOB_RANGES[index] / 127.0).round() as u16;
    let p = &mut inst.params;
    match index {
        0 => p.rate = scaled,
        1 => p.crush = scaled,
        2 => p.attack = scaled,
        3 => p.release = scaled,
        4 => p.grain = scaled,
        5 => p.shift = scaled,
        6 => p.start = scaled,
        _ => p.end = scaled,
    }
}
