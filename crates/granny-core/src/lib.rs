//! A port of the Bastl microGranny 2.5 granular sampler engine, made polyphonic and microtonal.
//!
//! The microGranny is an 8-bit granular sampler built on an ATmega328: it plays raw bytes off an SD
//! card straight into a 12-bit DAC, makes grains by periodically re-seeking the read head, and
//! crushes by OR-ing a mask into the DAC word. This crate reproduces those algorithms — including
//! the parts that are arguably bugs, because they are what the instrument sounds like — and adds
//! the polyphony, per-voice expression and tuning the hardware has no way to express.
//!
//! Reference: `bastl-instruments/bastlMicroGranny`, `examples/microGranny2_5/*.ino` and
//! `WaveRP.cpp`. Firmware by Václav Pelousek for Bastl Instruments.
//!
//! # Layout
//!
//! - [`tables`], [`curves`], [`rng`] — the firmware's constants and the arithmetic over them
//! - [`dac`], [`envelope`] — the output stage
//! - [`sample`] — the loaded audio, addressed the way the firmware addresses its SD card
//! - [`voice`], [`engine`] — one read head, and the pool of them
//! - [`pitch`] — working out what pitch a sample already is, so playback can track the keyboard
//! - [`params`] — the eight knobs, the setting bits, and the modulation matrix
//! - [`tuning`], [`scala`] — notes to playback rates, including microtonal ones
//!
//! # Where this deliberately differs from the hardware
//!
//! Each of these is a switch, and each defaults to the hardware's behaviour:
//!
//! - [`curves::CurveMode`] — the grain size and shift curves overflow in 16-bit arithmetic on AVR,
//!   so parts of the shift knob fold back on the real instrument
//! - [`params::Fidelity::interpolate`] — the hardware drops and repeats samples when transposing
//! - [`params::Fidelity::quantize_seeks`] — every seek is floored to a 512-byte SD block
//! - [`params::Fidelity::grain_fade_ms`] — grains start with a hard discontinuity
//!
//! And these have no hardware equivalent at all: polyphony, the per-voice modulation matrix, Scala
//! tuning, and the RATE knob acting as a transpose in pitch mode (where the hardware ignores it).

pub mod curves;
pub mod dac;
pub mod engine;
pub mod envelope;
pub mod params;
pub mod pitch;
pub mod rng;
pub mod sample;
pub mod scala;
pub mod tables;
pub mod tuning;
pub mod voice;

pub use engine::{fallback_voice_id, Engine, Scene, TransportInfo};
pub use params::{
    Expression, Fidelity, HwParams, ModDest, ModSlot, ModSource, Resolved, MOD_SLOTS,
};
pub use sample::SampleBuffer;
pub use tuning::{PitchTable, Tuning};
pub use voice::{Voice, VoiceKey};
