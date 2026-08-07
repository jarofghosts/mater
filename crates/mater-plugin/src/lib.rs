//! Mater — a polyphonic, MPE, microtonal CLAP port of the Bastl microGranny 2.5.
//!
//! One plugin instance stands in for one hardware preset: a single sample, one set of the eight
//! knob values, one set of setting bits. The DSP lives in `granny-core`; this crate is the CLAP
//! wrapper around it — parameters, voice management, MPE, sample loading and the editor.

use granny_core::params::{Fidelity, HwParams, ModSlot, MOD_SLOTS};
use granny_core::sample::SampleBuffer;
use granny_core::scala::ScalaTuning;
use granny_core::{Engine, Expression, Scene, TransportInfo, Tuning, VoiceKey};
use nih_plug::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

mod editor;
mod loader;
mod mpe;
mod params;
mod project;
mod shared;

use loader::StoredScale;
use mpe::{MpeState, Target, CC_SLIDE, CC_SUSTAIN};
use params::{MaterParams, NoteMode};
use shared::{Shared, MAX_VOICES, PLAYHEAD_IDLE};

/// Largest run of samples rendered without re-reading parameters.
const MAX_BLOCK_SIZE: usize = 128;

/// 24-ppq pulses per quarter note, the resolution the firmware counts MIDI clock in.
const TICKS_PER_BEAT: f64 = 24.0;

/// The first CC of the hardware's parameter map. CC102..=109 are the eight knobs, in order.
const HARDWARE_CC_BASE: u8 = 102;

/// Work that must not happen on the audio thread.
pub enum Task {
    LoadSample(PathBuf),
    LoadScl(PathBuf),
    LoadKbm(PathBuf),
    ClearScale,
}

pub struct Mater {
    params: Arc<MaterParams>,
    shared: Arc<Shared>,
    engine: Engine,

    /// The audio thread's own handle on the sample. Swapped, never mutated in place.
    sample: Arc<SampleBuffer>,
    scala: Option<Arc<ScalaTuning>>,

    mpe: MpeState,
    sustain: bool,
    /// Absolute values received on the hardware's CC map, when that is enabled.
    cc_overrides: [Option<u16>; granny_core::params::KNOBS],
}

impl Default for Mater {
    fn default() -> Self {
        let shared = Arc::new(Shared::default());
        Self {
            params: Arc::new(MaterParams::new(shared.clone())),
            shared,
            engine: Engine::new(MAX_VOICES, 44_100.0),
            sample: Arc::new(SampleBuffer::default()),
            scala: None,
            mpe: MpeState::default(),
            sustain: false,
            cc_overrides: [None; granny_core::params::KNOBS],
        }
    }
}

impl Mater {
    /// Take anything the main thread has left for us. Lock-free from the audio thread's point of
    /// view: if the lock is busy we simply try again next block.
    fn poll_handoff(&mut self) {
        let Some(mut handoff) = self.shared.handoff.try_lock() else {
            return;
        };

        if let Some(sample) = handoff.incoming_sample.take() {
            let retired = std::mem::replace(&mut self.sample, sample);
            // Hand the old buffer back so the main thread does the deallocation.
            if handoff.retired.len() < handoff.retired.capacity().max(8) {
                handoff.retired.push(retired);
            }
        }

        if let Some(scala) = handoff.incoming_scala.take() {
            let retired = std::mem::replace(&mut self.scala, scala);
            if let Some(retired) = retired {
                handoff.retired_scala.push(retired);
            }
        }
    }

    /// Read the current parameter values into the engine's own parameter struct.
    fn hardware_params(&self) -> HwParams {
        let p = &self.params;
        let knob = |index: usize, value: i32, max: u16| -> u16 {
            match self.cc_overrides[index] {
                Some(override_value) if p.hardware_cc.value() => override_value.min(max),
                _ => value.clamp(0, max as i32) as u16,
            }
        };

        HwParams {
            rate: knob(0, p.rate.value(), 1023),
            crush: knob(1, p.crush.value(), 127),
            attack: knob(2, p.attack.value(), 127),
            release: knob(3, p.release.value(), 127),
            grain: knob(4, p.grain.value(), 127),
            shift: knob(5, p.shift.value(), 255),
            start: knob(6, p.start.value(), 1023),
            end: knob(7, p.end.value(), 1023),

            tuned: p.note_mode.value() == NoteMode::Pitch,
            legato: p.legato.value(),
            repeat: p.repeat.value(),
            sync: p.sync.value(),
            random_shift: p.random_shift.value(),

            hold: p.hold.value(),
            level: p.level.smoothed.next(),
            vel_sensitivity: p.vel_sensitivity.value(),
        }
    }

    fn fidelity(&self) -> Fidelity {
        Fidelity {
            curve_mode: self.params.curve_mode(),
            interpolate: self.params.interpolate.value(),
            quantize_seeks: self.params.quantize_seeks.value(),
            grain_fade_ms: self.params.grain_fade.value(),
        }
    }

    fn mod_matrix(&self) -> [ModSlot; MOD_SLOTS] {
        std::array::from_fn(|i| {
            let row = &self.params.mods[i];
            ModSlot {
                source: row.source.value().to_core(),
                dest: row.dest.value().to_core(),
                depth: row.depth.value(),
            }
        })
    }

    /// The pitch the loaded sample already sounds at, which is what playback is transposed from.
    ///
    /// With matching off, or with nothing detectable in the sample, this is the native note, and
    /// the plugin behaves like the hardware: the sample plays at its recorded speed and whatever
    /// pitch that happens to be is what you hear.
    fn root_note(&self) -> f32 {
        let detected = if self.params.match_input_pitch.value() {
            self.sample.detected_root().map(|d| d.note)
        } else {
            None
        };
        detected.unwrap_or(granny_core::tables::NATIVE_NOTE) + self.params.root_adjust.value()
    }

    fn tuning(&self) -> Tuning {
        Tuning {
            table: self.params.pitch_table(),
            snap_divisions: self.params.snap_divisions(),
            scala: self.params.active_scala(&self.scala),
            root_note: self.root_note(),
        }
    }

    /// Build the per-voice expression a new note should start with, from its channel's MPE state.
    fn expression_for(&self, channel: u8) -> Expression {
        Expression {
            bend: self.mpe.bend_for(channel),
            bend_semitones: self.mpe.bend_semitones_for(channel),
            pressure: self.mpe.pressure_for(channel),
            slide: self.mpe.slide_for(channel),
            ..Default::default()
        }
    }

    fn apply_to_target(&mut self, target: Target, apply: impl Fn(&mut Expression)) {
        match target {
            Target::All => self.engine.for_each_matching(None, None, None, apply),
            Target::Channel(channel) => {
                self.engine
                    .for_each_matching(None, Some(channel), None, apply)
            }
        }
    }

    fn handle_event(&mut self, scene: &Scene, event: NoteEvent<()>) -> Option<VoiceKey> {
        match event {
            NoteEvent::NoteOn {
                voice_id,
                channel,
                note,
                velocity,
                ..
            } => {
                let key = VoiceKey {
                    voice_id: voice_id
                        .unwrap_or_else(|| granny_core::fallback_voice_id(channel, note)),
                    channel,
                    note,
                };
                return self
                    .engine
                    .note_on(scene, key, velocity, self.expression_for(channel));
            }
            NoteEvent::NoteOff {
                voice_id,
                channel,
                note,
                ..
            } => self.engine.note_off(voice_id, Some(channel), Some(note)),
            NoteEvent::Choke {
                voice_id,
                channel,
                note,
                ..
            } => self.engine.choke(voice_id, Some(channel), Some(note)),

            // --- CLAP note expressions: per-voice by construction ---
            NoteEvent::PolyTuning {
                voice_id,
                channel,
                note,
                tuning,
                ..
            } => self
                .engine
                .for_each_matching(voice_id, Some(channel), Some(note), |expression| {
                    expression.note_tuning = tuning
                }),
            NoteEvent::PolyPressure {
                voice_id,
                channel,
                note,
                pressure,
                ..
            } => self
                .engine
                .for_each_matching(voice_id, Some(channel), Some(note), |expression| {
                    expression.pressure = pressure
                }),
            NoteEvent::PolyBrightness {
                voice_id,
                channel,
                note,
                brightness,
                ..
            } => self
                .engine
                .for_each_matching(voice_id, Some(channel), Some(note), |expression| {
                    expression.slide = brightness
                }),
            NoteEvent::PolyVolume {
                voice_id,
                channel,
                note,
                gain,
                ..
            } => self
                .engine
                .for_each_matching(voice_id, Some(channel), Some(note), |expression| {
                    expression.gain = gain
                }),
            NoteEvent::PolyPan {
                voice_id,
                channel,
                note,
                pan,
                ..
            } => self
                .engine
                .for_each_matching(voice_id, Some(channel), Some(note), |expression| {
                    expression.pan = pan
                }),

            // --- CLAP polyphonic modulation of the eight knobs ---
            NoteEvent::PolyModulation {
                voice_id,
                poly_modulation_id,
                normalized_offset,
                ..
            } => {
                let index = poly_modulation_id as usize;
                if index < granny_core::params::KNOBS {
                    self.engine
                        .for_each_matching(Some(voice_id), None, None, |expression| {
                            expression.poly_mod[index] = normalized_offset
                        });
                } else {
                    nih_debug_assert_failure!("unknown poly modulation id {}", poly_modulation_id);
                }
            }
            // The base value is re-read from the parameter every control tick, so a change to the
            // host's automation is already reflected and the per-voice offsets stay valid.
            NoteEvent::MonoAutomation { .. } => {}

            // --- plain MIDI channel messages ---
            NoteEvent::MidiPitchBend { channel, value, .. } => {
                let target = self.mpe.set_bend(channel, value);
                let bend = self.mpe.bend_for(channel);
                let semitones = self.mpe.bend_semitones_for(channel);
                match target {
                    Target::All => {
                        // Every voice takes its own channel's resolved bend, not this one's.
                        let mpe = self.mpe.clone();
                        self.engine.for_each_all(|channel, expression| {
                            expression.bend = mpe.bend_for(channel);
                            expression.bend_semitones = mpe.bend_semitones_for(channel);
                        });
                    }
                    Target::Channel(_) => self.apply_to_target(target, |expression| {
                        expression.bend = bend;
                        expression.bend_semitones = semitones;
                    }),
                }
            }
            NoteEvent::MidiChannelPressure {
                channel, pressure, ..
            } => {
                let target = self.mpe.set_pressure(channel, pressure);
                let pressure = self.mpe.pressure_for(channel);
                self.apply_to_target(target, |expression| expression.pressure = pressure);
            }
            NoteEvent::MidiCC {
                channel, cc, value, ..
            } => self.handle_cc(channel, cc, value),

            _ => {}
        }
        None
    }

    fn handle_cc(&mut self, channel: u8, cc: u8, value: f32) {
        if let Some(range) = self.mpe.handle_rpn(channel, cc, value) {
            nih_log!("pitch bend range set to {range} semitones by RPN 0");
        }

        match cc {
            CC_SLIDE => {
                let target = self.mpe.set_slide(channel, value);
                let slide = self.mpe.slide_for(channel);
                self.apply_to_target(target, |expression| expression.slide = slide);
            }
            CC_SUSTAIN => {
                self.sustain = value >= 0.5;
                self.engine.set_sustain(self.sustain);
            }
            // The hardware's own CC map, so an existing microGranny rig drives the plugin. These
            // override the parameter values rather than automating them, because a plugin cannot
            // write to its own parameters from the audio thread.
            _ if self.params.hardware_cc.value() => {
                let raw = (value * 127.0).round().clamp(0.0, 127.0) as u16;
                match cc {
                    // Modwheel is wired to crush on 2.5.
                    1 => self.cc_overrides[1] = Some(raw),
                    _ if (HARDWARE_CC_BASE..HARDWARE_CC_BASE + 8).contains(&cc) => {
                        let index = (cc - HARDWARE_CC_BASE) as usize;
                        let range = granny_core::params::KNOB_RANGES[index];
                        // The firmware scales the 7-bit CC up to each parameter's bit depth.
                        self.cc_overrides[index] =
                            Some((raw as f32 * range / 127.0).round() as u16);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn publish_playheads(&self) {
        for (index, slot) in self.shared.playheads.iter().enumerate() {
            let position = match self.engine.voices().get(index) {
                Some(voice) if voice.active => voice.position_fraction(&self.sample),
                _ => PLAYHEAD_IDLE,
            };
            slot.store(position, Ordering::Relaxed);
        }
    }
}

impl Plugin for Mater {
    const NAME: &'static str = "mater";
    const VENDOR: &'static str = "grimoire.supply";
    const URL: &'static str = "https://github.com/jarofghosts/mater";
    const EMAIL: &'static str = "decapitron@gmail.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: std::num::NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    // MidiCCs rather than Basic: MPE needs per-channel bend, pressure and CC74.
    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = Task;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let params = self.params.clone();
        let shared = self.shared.clone();

        Box::new(move |task| match task {
            Task::LoadSample(path) => match loader::load_file(&path, params.load_options()) {
                Ok(sample) => {
                    let message = format!("loaded {} ({} bytes)", sample.name, sample.data.len());
                    params.sample.store(sample);
                    shared.set_status(message);
                }
                Err(err) => shared.set_status(format!("could not load sample: {err}")),
            },
            Task::LoadScl(path) => match loader::load_text(&path) {
                Ok((name, text)) => {
                    let result = params.scale.update(|scale| {
                        scale.scl_name = name.clone();
                        scale.scl = text;
                    });
                    match result {
                        Ok(()) => shared.set_status(format!("loaded scale {name}")),
                        Err(err) => shared.set_status(format!("bad scale: {err}")),
                    }
                }
                Err(err) => shared.set_status(format!("could not read scale: {err}")),
            },
            Task::LoadKbm(path) => match loader::load_text(&path) {
                Ok((name, text)) => {
                    let result = params.scale.update(|scale| {
                        scale.kbm_name = name.clone();
                        scale.kbm = text;
                    });
                    match result {
                        Ok(()) => shared.set_status(format!("loaded keyboard map {name}")),
                        Err(err) => shared.set_status(format!("bad keyboard map: {err}")),
                    }
                }
                Err(err) => shared.set_status(format!("could not read keyboard map: {err}")),
            },
            Task::ClearScale => match params.scale.store(StoredScale::default()) {
                Ok(()) => shared.set_status("cleared scala tuning"),
                Err(err) => shared.set_status(err),
            },
        })
    }

    fn editor(&mut self, async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.shared.clone(), async_executor)
    }

    fn initialize(
        &mut self,
        _layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        self.engine.set_sample_rate(buffer_config.sample_rate);
        self.engine
            .set_voice_capacity(self.params.voices.value() as usize);
        context.set_current_voice_capacity(self.params.voices.value() as u32);
        // State may have been restored before we were activated.
        self.poll_handoff();
        true
    }

    fn reset(&mut self) {
        self.engine.reset();
        self.mpe = MpeState::default();
        self.sustain = false;
        self.cc_overrides = [None; granny_core::params::KNOBS];
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.poll_handoff();

        let requested_voices = self.params.voices.value() as usize;
        if requested_voices != self.engine.capacity() {
            self.engine.set_voice_capacity(requested_voices);
            context.set_current_voice_capacity(requested_voices as u32);
        }
        self.mpe.set_zone(self.params.mpe_zone.value());
        self.mpe.set_ranges(
            self.params.bend_range.value() as f32,
            self.params.master_bend_range.value() as f32,
            self.params.follow_rpn.value(),
        );

        // Copy what we need out of the transport so `context` stays free for events.
        let (playing, ticks_per_sample, block_start_ticks) = {
            let transport = context.transport();
            let sample_rate = transport.sample_rate as f64;
            (
                transport.playing,
                transport
                    .tempo
                    .map(|tempo| tempo / 60.0 * TICKS_PER_BEAT / sample_rate)
                    .unwrap_or(0.0),
                transport.pos_beats().unwrap_or(0.0) * TICKS_PER_BEAT,
            )
        };

        let sample = self.sample.clone();
        let fidelity = self.fidelity();
        let mods = self.mod_matrix();

        let num_samples = buffer.samples();
        let mut next_event = context.next_event();
        let mut block_start = 0usize;

        while block_start < num_samples {
            let mut block_end = (block_start + MAX_BLOCK_SIZE).min(num_samples);

            // Parameters are re-read for every sub-block, so automation and CC land on time.
            let hardware = self.hardware_params();
            let tuning = self.tuning();
            let scene = Scene {
                sample: &sample,
                params: &hardware,
                fidelity: &fidelity,
                tuning: &tuning,
                mods: &mods,
                transport: TransportInfo {
                    playing,
                    pos_ticks: block_start_ticks + block_start as f64 * ticks_per_sample,
                    ticks_per_sample,
                },
            };

            'events: loop {
                match next_event {
                    Some(event) if (event.timing() as usize) <= block_start => {
                        let timing = event.timing();
                        if let Some(stolen) = self.handle_event(&scene, event) {
                            context.send_event(NoteEvent::VoiceTerminated {
                                timing,
                                voice_id: Some(stolen.voice_id),
                                channel: stolen.channel,
                                note: stolen.note,
                            });
                        }
                        next_event = context.next_event();
                    }
                    // Cut the sub-block short so the next event lands on the right sample.
                    Some(event) if (event.timing() as usize) < block_end => {
                        block_end = event.timing() as usize;
                        break 'events;
                    }
                    _ => break 'events,
                }
            }

            {
                let output = buffer.as_slice();
                let (left_channel, rest) = output.split_at_mut(1);
                let left = &mut left_channel[0][block_start..block_end];
                let right = &mut rest[0][block_start..block_end];
                left.fill(0.0);
                right.fill(0.0);
                self.engine.process(&scene, left, right);
            }

            let timing = block_start as u32;
            let mut terminated: [Option<VoiceKey>; MAX_VOICES] = [None; MAX_VOICES];
            let mut count = 0;
            self.engine.drain_terminated(|key| {
                if count < terminated.len() {
                    terminated[count] = Some(key);
                    count += 1;
                }
            });
            for key in terminated.iter().flatten() {
                context.send_event(NoteEvent::VoiceTerminated {
                    timing,
                    voice_id: Some(key.voice_id),
                    channel: key.channel,
                    note: key.note,
                });
            }

            block_start = block_end;
        }

        self.publish_playheads();

        ProcessStatus::Normal
    }
}

impl ClapPlugin for Mater {
    const CLAP_ID: &'static str = "supply.grimoire.mater";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("polyphonic, mpe and microtonal granular sampler");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Instrument,
        ClapFeature::Sampler,
        ClapFeature::Stereo,
    ];
    const CLAP_POLY_MODULATION_CONFIG: Option<PolyModulationConfig> = Some(PolyModulationConfig {
        max_voice_capacity: MAX_VOICES as u32,
        supports_overlapping_voices: true,
    });
}

nih_export_clap!(Mater);
