//! What a quarter tone from a controller actually does to the playback rate.
//!
//! The MPE layer has its own tests, but nothing covered the path from a bend arriving to the rate
//! the DAC is clocked at, which is the part that decides whether a quarter tone is audible as one.
//! These drive the real `MpeState` and the real `Voice`, so the whole chain is under test: bend
//! units -> semitones -> pitch offset -> effective note -> rate.

use granny_core::rng::Xorshift96;
use granny_core::voice::{TickCtx, Transport};
use granny_core::{
    Expression, Fidelity, HwParams, ModSlot, MpeState, MpeZone, PitchTable, SampleBuffer, Tuning,
    Voice, VoiceKey, MOD_SLOTS,
};

/// A LinnStrument in quartertone bend mode sends this, on a member channel, at bend range 48.
const QUARTERTONE_BEND_UNITS: i32 = 85;
const MEMBER_RANGE: f32 = 48.0;
const MEMBER_CHANNEL: u8 = 5;

/// Bend units either side of centre, as the 14-bit value a pitch bend message carries.
fn normalized_bend(offset_units: i32) -> f32 {
    0.5 + offset_units as f32 / 16384.0
}

/// Semitones the module resolves a bend to, through the same state machine `on_midi` uses.
fn semitones_for(offset_units: i32) -> f32 {
    let mut mpe = MpeState::default();
    mpe.set_zone(MpeZone::Lower);
    mpe.set_ranges(MEMBER_RANGE, 2.0, false);
    mpe.set_bend(MEMBER_CHANNEL, normalized_bend(offset_units));
    mpe.bend_semitones_for(MEMBER_CHANNEL)
}

/// Playback rate for a note carrying a given pitch offset, through the real voice.
fn rate_hz(note: u8, bend_semitones: f32, snap_divisions: Option<u32>) -> f32 {
    let sample = SampleBuffer::new("test", vec![128u8; 4096], 22050);
    let params = HwParams::default();
    let fidelity = Fidelity::default();
    let mods = [ModSlot::default(); MOD_SLOTS];
    let tuning = Tuning {
        table: PitchTable::EqualTemperament,
        snap_divisions,
        scala: None,
        ..Tuning::default()
    };
    let ctx = TickCtx {
        sample: &sample,
        params: &params,
        fidelity: &fidelity,
        tuning: &tuning,
        mods: &mods,
        transport: Transport::default(),
        now_ms: 0,
        sample_rate: 44100.0,
    };
    let mut voice = Voice::default();
    let mut expr = Expression::default();
    expr.bend_semitones = bend_semitones;
    voice.start(
        VoiceKey { voice_id: 0, channel: MEMBER_CHANNEL, note },
        1,
        1.0,
        expr,
        &ctx,
        &mut Xorshift96::new(),
    );
    voice.rate_hz()
}

fn cents(from: f32, to: f32) -> f32 {
    1200.0 * (to / from).log2()
}

#[test]
fn a_semitone_of_note_number_is_one_hundred_cents() {
    let step = cents(rate_hz(55, 0.0, None), rate_hz(56, 0.0, None));
    assert!(
        (step - 100.0).abs() < 0.5,
        "one note number apart should be 100 cents, got {step:.2}"
    );
}

#[test]
fn the_bend_resolves_to_half_a_semitone() {
    let st = semitones_for(-QUARTERTONE_BEND_UNITS);
    assert!(
        (st + 0.5).abs() < 0.01,
        "-{QUARTERTONE_BEND_UNITS} units at range {MEMBER_RANGE} should be half a semitone down, got {st:.4}"
    );
}

#[test]
fn a_quartertone_cell_sounds_fifty_cents_below_the_note_it_reports() {
    // The cell reports the note number of the semitone to its right, then bends down into place, so
    // the two have to come out a quarter tone apart and not a semitone or nothing at all.
    let semitone = rate_hz(56, 0.0, None);
    let quartertone = rate_hz(56, semitones_for(-QUARTERTONE_BEND_UNITS), None);
    let gap = cents(quartertone, semitone);
    assert!(
        (gap - 50.0).abs() < 1.0,
        "quarter tone cell should sit 50 cents below its note number, got {gap:.2}"
    );
}

#[test]
fn the_quartertone_lands_halfway_between_its_neighbours() {
    let left = rate_hz(55, 0.0, None);
    let middle = rate_hz(56, semitones_for(-QUARTERTONE_BEND_UNITS), None);
    let right = rate_hz(56, 0.0, None);
    let a = cents(left, middle);
    let b = cents(middle, right);
    assert!(
        (a - 50.0).abs() < 1.0 && (b - 50.0).abs() < 1.0,
        "three adjacent cells should step 50 cents each, got {a:.2} then {b:.2}"
    );
}

#[test]
fn snapping_to_24_edo_makes_the_quarter_tone_exact() {
    // Snapping is applied after the offset, so it should absorb an inexact bend rather than being
    // defeated by one. A deliberately wrong bend still has to land on the quarter tone grid.
    let sloppy = semitones_for(-QUARTERTONE_BEND_UNITS) * 0.6;
    let semitone = rate_hz(56, 0.0, Some(24));
    let quartertone = rate_hz(56, sloppy, Some(24));
    let gap = cents(quartertone, semitone);
    assert!(
        (gap - 50.0).abs() < 0.01,
        "snap to 24-edo should place it exactly 50 cents below, got {gap:.4}"
    );
}
