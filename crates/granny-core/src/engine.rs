//! The voice pool and the control clock.
//!
//! The firmware's main loop reads MIDI, runs the UI and calls `updateSound` over and over. Its
//! envelope and grain timing are quantised to whole milliseconds by `millis()`, so this engine
//! drives a **1 ms control tick** independent of the host's block size. Everything between ticks is
//! pure sample playback. That is what keeps a 4 ms grain sounding like a 4 ms grain regardless of
//! whether the host hands us 64 or 2048 samples at a time.

use crate::params::{Expression, Fidelity, HwParams, ModSlot};
use crate::rng::Xorshift96;
use crate::sample::SampleBuffer;
use crate::tuning::Tuning;
use crate::voice::{TickCtx, Transport, Voice, VoiceKey};

/// Control ticks per second. The firmware's timing resolution is `millis()`.
const CONTROL_HZ: f32 = 1000.0;

/// Host transport as the plugin sees it, before it is resolved to a tick per control step.
#[derive(Copy, Clone, Debug, Default)]
pub struct TransportInfo {
    pub playing: bool,
    /// Position at the start of the block, in 24-ppq ticks.
    pub pos_ticks: f64,
    /// How far the 24-ppq clock advances per audio sample.
    pub ticks_per_sample: f64,
}

/// Everything the engine needs that lives outside it.
pub struct Scene<'a> {
    pub sample: &'a SampleBuffer,
    pub params: &'a HwParams,
    pub fidelity: &'a Fidelity,
    pub tuning: &'a Tuning,
    pub mods: &'a [ModSlot],
    pub transport: TransportInfo,
}

pub struct Engine {
    voices: Vec<Voice>,
    sample_rate: f32,
    samples_per_tick: f32,
    /// Samples remaining before the next control tick.
    samples_to_tick: f32,
    now_ms: u64,
    next_internal_id: u64,
    rng: Xorshift96,
    sustain: bool,
    /// Voices that ended since the host last looked, so it can be told about them.
    terminated: Vec<VoiceKey>,
}

impl Engine {
    pub fn new(max_voices: usize, sample_rate: f32) -> Self {
        let mut engine = Self {
            voices: vec![Voice::default(); max_voices.max(1)],
            sample_rate: 44100.0,
            samples_per_tick: 44.1,
            samples_to_tick: 0.0,
            now_ms: 0,
            next_internal_id: 0,
            rng: Xorshift96::new(),
            sustain: false,
            // Generous, so the audio thread never has to grow it mid-block.
            terminated: Vec::with_capacity(max_voices.max(1) * 4),
        };
        engine.set_sample_rate(sample_rate);
        engine
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.samples_per_tick = (self.sample_rate / CONTROL_HZ).max(1.0);
        self.samples_to_tick = self.samples_per_tick;
    }

    pub fn set_voice_capacity(&mut self, max_voices: usize) {
        self.voices.resize(max_voices.max(1), Voice::default());
    }

    pub fn capacity(&self) -> usize {
        self.voices.len()
    }

    pub fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.choke();
        }
        // Nothing to report: the host is resetting us, it is not a musical voice ending.
        self.terminated.clear();
        self.samples_to_tick = self.samples_per_tick;
        self.now_ms = 0;
        self.next_internal_id = 0;
        self.rng = Xorshift96::new();
        self.sustain = false;
    }

    pub fn voices(&self) -> &[Voice] {
        &self.voices
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    pub fn set_sustain(&mut self, on: bool) {
        self.sustain = on;
    }

    fn ctx<'a>(&self, scene: &Scene<'a>, tick: i64) -> TickCtx<'a> {
        TickCtx {
            sample: scene.sample,
            params: scene.params,
            fidelity: scene.fidelity,
            tuning: scene.tuning,
            mods: scene.mods,
            transport: Transport {
                playing: scene.transport.playing,
                tick,
            },
            now_ms: self.now_ms,
            sample_rate: self.sample_rate,
        }
    }

    fn tick_at(&self, scene: &Scene, sample_offset: usize) -> i64 {
        (scene.transport.pos_ticks + sample_offset as f64 * scene.transport.ticks_per_sample)
            .floor() as i64
    }

    /// Start a note. Returns the identity of a voice that had to be stolen, if any, so the plugin
    /// can tell the host about it.
    pub fn note_on(
        &mut self,
        scene: &Scene,
        key: VoiceKey,
        velocity: f32,
        expr: Expression,
    ) -> Option<VoiceKey> {
        let ctx = self.ctx(scene, self.tick_at(scene, 0));

        // LEGATO: retune the voice already sounding on this channel rather than starting another.
        if scene.params.legato {
            if let Some(voice) = self
                .voices
                .iter_mut()
                .find(|v| v.active && v.held && v.key.channel == key.channel)
            {
                voice.key.voice_id = key.voice_id;
                voice.relegato(key.note, &ctx, &mut self.rng);
                return None;
            }
        }

        let (index, stolen) = match self.voices.iter().position(|v| !v.active) {
            Some(index) => (index, None),
            None => {
                // Steal the oldest voice, preferring ones that are already releasing.
                let index = self
                    .voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| (v.held, v.internal_id))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                (index, Some(self.voices[index].key))
            }
        };

        let internal_id = self.next_internal_id;
        self.next_internal_id = self.next_internal_id.wrapping_add(1);
        self.voices[index].start(key, internal_id, velocity, expr, &ctx, &mut self.rng);

        stolen
    }

    /// Mark matching voices as released. `None` fields match anything, as CLAP allows.
    pub fn note_off(&mut self, voice_id: Option<i32>, channel: Option<u8>, note: Option<u8>) {
        for voice in self.voices.iter_mut().filter(|v| v.active) {
            if matches(voice, voice_id, channel, note) {
                voice.key_up();
            }
        }
    }

    /// Stop matching voices immediately, without a release.
    pub fn choke(&mut self, voice_id: Option<i32>, channel: Option<u8>, note: Option<u8>) {
        let terminated = &mut self.terminated;
        for voice in self.voices.iter_mut().filter(|v| v.active) {
            if matches(voice, voice_id, channel, note) {
                voice.choke();
                push_terminated(terminated, voice.key);
            }
        }
    }

    /// Hand back every voice that has ended since the last call, then forget them.
    pub fn drain_terminated(&mut self, mut report: impl FnMut(VoiceKey)) {
        for key in self.terminated.drain(..) {
            report(key);
        }
    }

    pub fn all_notes_off(&mut self) {
        for voice in &mut self.voices {
            voice.key_up();
        }
    }

    /// Apply a change to every voice that matches, for expression events.
    pub fn for_each_matching(
        &mut self,
        voice_id: Option<i32>,
        channel: Option<u8>,
        note: Option<u8>,
        mut f: impl FnMut(&mut Expression),
    ) {
        for voice in self.voices.iter_mut().filter(|v| v.active) {
            if matches(voice, voice_id, channel, note) {
                f(&mut voice.expr);
            }
        }
    }

    /// Apply a change to every sounding voice, told which channel each one is on.
    ///
    /// Needed for a master-channel MPE message, where each voice's resolved value depends on its
    /// own member channel as well as the master's.
    pub fn for_each_all(&mut self, mut f: impl FnMut(u8, &mut Expression)) {
        for voice in self.voices.iter_mut().filter(|v| v.active) {
            let channel = voice.key.channel;
            f(channel, &mut voice.expr);
        }
    }

    /// Render a block, running control ticks at the right sample offsets.
    ///
    /// Adds into the output slices, so the caller must clear them first.
    pub fn process(&mut self, scene: &Scene, left: &mut [f32], right: &mut [f32]) {
        let num_samples = left.len().min(right.len());
        let mut offset = 0;

        while offset < num_samples {
            let until_tick = self.samples_to_tick.ceil().max(1.0) as usize;
            let run = until_tick.min(num_samples - offset);

            for voice in self.voices.iter_mut().filter(|v| v.active) {
                voice.render(
                    scene.sample,
                    scene.fidelity,
                    &mut left[offset..offset + run],
                    &mut right[offset..offset + run],
                );
            }

            offset += run;
            self.samples_to_tick -= run as f32;

            while self.samples_to_tick <= 0.0 {
                self.samples_to_tick += self.samples_per_tick;
                self.now_ms += 1;
                let tick = self.tick_at(scene, offset);
                self.control_tick(scene, tick);
            }
        }
    }

    fn control_tick(&mut self, scene: &Scene, tick: i64) {
        let ctx = self.ctx(scene, tick);
        let deferred = scene.params.hold || self.sustain;
        let now_ms = self.now_ms;

        // Split the borrows so terminations can be recorded while iterating the pool.
        let rng = &mut self.rng;
        let terminated = &mut self.terminated;

        for voice in self.voices.iter_mut() {
            if !voice.active {
                continue;
            }
            if !voice.held && !deferred {
                voice.release(now_ms);
            }
            voice.control_tick(&ctx, rng);
            if !voice.active {
                push_terminated(terminated, voice.key);
            }
        }
    }
}

/// Record an ended voice without ever growing the buffer on the audio thread.
fn push_terminated(terminated: &mut Vec<VoiceKey>, key: VoiceKey) {
    if terminated.len() < terminated.capacity() {
        terminated.push(key);
    }
}

fn matches(voice: &Voice, voice_id: Option<i32>, channel: Option<u8>, note: Option<u8>) -> bool {
    // A host that sends a voice id expects it to be authoritative.
    if let Some(id) = voice_id {
        return voice.key.voice_id == id;
    }
    channel.is_none_or(|c| voice.key.channel == c) && note.is_none_or(|n| voice.key.note == n)
}

/// The fallback voice id CLAP hosts expect when they do not supply one themselves.
pub fn fallback_voice_id(channel: u8, note: u8) -> i32 {
    i32::from(note) | (i32::from(channel) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::MOD_SLOTS;

    fn sample(len: usize) -> SampleBuffer {
        // A ramp, so position changes are audible in the output.
        let data: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
        SampleBuffer::new("ramp", data, 22050)
    }

    struct Fixture {
        sample: SampleBuffer,
        params: HwParams,
        fidelity: Fidelity,
        tuning: Tuning,
        mods: [ModSlot; MOD_SLOTS],
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                sample: sample(200_000),
                params: HwParams::default(),
                fidelity: Fidelity::default(),
                tuning: Tuning::default(),
                mods: [ModSlot::default(); MOD_SLOTS],
            }
        }

        fn scene(&self) -> Scene<'_> {
            Scene {
                sample: &self.sample,
                params: &self.params,
                fidelity: &self.fidelity,
                tuning: &self.tuning,
                mods: &self.mods,
                transport: TransportInfo::default(),
            }
        }
    }

    fn key(note: u8) -> VoiceKey {
        VoiceKey {
            voice_id: fallback_voice_id(0, note),
            channel: 0,
            note,
        }
    }

    fn render(engine: &mut Engine, fixture: &Fixture, samples: usize) -> (Vec<f32>, Vec<f32>) {
        let mut left = vec![0.0; samples];
        let mut right = vec![0.0; samples];
        engine.process(&fixture.scene(), &mut left, &mut right);
        (left, right)
    }

    #[test]
    fn a_note_produces_audio() {
        let fixture = Fixture::new();
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        let (left, _) = render(&mut engine, &fixture, 4_800);
        assert!(left.iter().any(|s| s.abs() > 0.01), "expected signal");
    }

    #[test]
    fn silence_without_a_note() {
        let fixture = Fixture::new();
        let mut engine = Engine::new(16, 48_000.0);
        let (left, right) = render(&mut engine, &fixture, 1_024);
        assert!(left.iter().all(|s| *s == 0.0));
        assert!(right.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn voices_are_independent_and_polyphonic() {
        let fixture = Fixture::new();
        let mut engine = Engine::new(16, 48_000.0);
        for note in [50, 55, 59, 64] {
            engine.note_on(&fixture.scene(), key(note), 1.0, Expression::default());
        }
        render(&mut engine, &fixture, 480);
        assert_eq!(engine.active_voices(), 4);

        let rates: Vec<f32> = engine
            .voices()
            .iter()
            .filter(|v| v.active)
            .map(|v| v.rate_hz())
            .collect();
        for pair in rates.windows(2) {
            assert!(pair[0] != pair[1], "each note should have its own rate");
        }
    }

    #[test]
    fn oldest_voice_is_stolen_when_the_pool_is_full() {
        let fixture = Fixture::new();
        let mut engine = Engine::new(2, 48_000.0);
        engine.note_on(&fixture.scene(), key(60), 1.0, Expression::default());
        engine.note_on(&fixture.scene(), key(62), 1.0, Expression::default());
        let stolen = engine.note_on(&fixture.scene(), key(64), 1.0, Expression::default());
        assert_eq!(stolen.map(|k| k.note), Some(60));
        assert_eq!(engine.active_voices(), 2);
    }

    #[test]
    fn note_off_releases_and_the_voice_eventually_frees() {
        let mut fixture = Fixture::new();
        fixture.params.release = 1;
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        render(&mut engine, &fixture, 480);
        assert_eq!(engine.active_voices(), 1);

        engine.note_off(None, Some(0), Some(59));
        // 36 release steps at 1 ms each, plus slack.
        render(&mut engine, &fixture, 48_000 / 10);
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn sustain_defers_the_release() {
        let mut fixture = Fixture::new();
        fixture.params.release = 1;
        let mut engine = Engine::new(16, 48_000.0);
        engine.set_sustain(true);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        engine.note_off(None, Some(0), Some(59));
        render(&mut engine, &fixture, 48_000 / 10);
        assert_eq!(engine.active_voices(), 1, "sustain should hold the voice");

        engine.set_sustain(false);
        render(&mut engine, &fixture, 48_000 / 10);
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn legato_retunes_instead_of_stacking_voices() {
        let mut fixture = Fixture::new();
        fixture.params.legato = true;
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        let before = engine.voices()[0].rate_hz();
        engine.note_on(&fixture.scene(), key(64), 1.0, Expression::default());
        assert_eq!(engine.active_voices(), 1, "legato should not add a voice");
        assert!(engine.voices()[0].rate_hz() > before);
    }

    #[test]
    fn choke_stops_a_voice_immediately() {
        let fixture = Fixture::new();
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        engine.choke(None, Some(0), Some(59));
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn playback_is_block_size_independent() {
        // The 1 ms control clock means grain and envelope timing must not depend on how the host
        // chops up the buffer.
        let mut fixture = Fixture::new();
        fixture.params.grain = 20;
        fixture.params.shift = 200;

        let render_with = |block: usize| {
            let mut engine = Engine::new(16, 48_000.0);
            engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
            let total = 24_000;
            let mut left = vec![0.0f32; total];
            let mut right = vec![0.0f32; total];
            let mut offset = 0;
            while offset < total {
                let run = block.min(total - offset);
                engine.process(
                    &fixture.scene(),
                    &mut left[offset..offset + run],
                    &mut right[offset..offset + run],
                );
                offset += run;
            }
            left
        };

        let a = render_with(64);
        let b = render_with(1024);
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert!((x - y).abs() < 1e-6, "sample {i}: {x} vs {y}");
        }
    }

    #[test]
    fn repeat_off_stops_at_the_end_of_the_loop() {
        let mut fixture = Fixture::new();
        fixture.params.repeat = false;
        fixture.params.end = 20; // a short window near the start
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        render(&mut engine, &fixture, 48_000);
        assert_eq!(engine.active_voices(), 0, "should have run out and stopped");
    }

    #[test]
    fn repeat_on_keeps_looping() {
        let mut fixture = Fixture::new();
        fixture.params.repeat = true;
        fixture.params.end = 20;
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        let (left, _) = render(&mut engine, &fixture, 48_000);
        assert_eq!(engine.active_voices(), 1);
        assert!(left.iter().any(|s| s.abs() > 0.01));
    }

    #[test]
    fn an_empty_sample_is_silent_and_frees_the_voice() {
        let mut fixture = Fixture::new();
        fixture.sample = SampleBuffer::default();
        let mut engine = Engine::new(16, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        let (left, _) = render(&mut engine, &fixture, 4_800);
        assert!(left.iter().all(|s| *s == 0.0));
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn bending_one_channel_moves_only_that_voice() {
        // The core of MPE: two notes, one bend, one voice moves.
        let fixture = Fixture::new();
        let mut engine = Engine::new(16, 48_000.0);
        for (channel, note) in [(1u8, 59u8), (2, 59)] {
            engine.note_on(
                &fixture.scene(),
                VoiceKey {
                    voice_id: fallback_voice_id(channel, note),
                    channel,
                    note,
                },
                1.0,
                Expression::default(),
            );
        }
        render(&mut engine, &fixture, 480);
        let baseline = engine.voices()[0].rate_hz();
        assert_eq!(engine.voices()[1].rate_hz(), baseline);

        // An octave of bend on channel 1 only.
        engine.for_each_matching(None, Some(1), None, |expression| {
            expression.bend_semitones = 12.0
        });
        render(&mut engine, &fixture, 480);

        let bent = engine.voices()[0].rate_hz();
        let untouched = engine.voices()[1].rate_hz();
        assert_eq!(untouched, baseline, "the other channel must not move");
        assert!(
            (bent / baseline - 2.0).abs() < 0.02,
            "expected an octave, got a ratio of {}",
            bent / baseline
        );
    }

    #[test]
    fn quarter_tone_bends_land_exactly_when_snapping_to_24_edo() {
        let mut fixture = Fixture::new();
        fixture.tuning = Tuning {
            snap_divisions: Some(24),
            ..Default::default()
        };
        let mut engine = Engine::new(4, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        render(&mut engine, &fixture, 240);
        let unbent = engine.voices()[0].rate_hz();

        // 60 cents of bend: past the halfway point, so it snaps up to the quarter tone.
        engine.for_each_matching(None, None, None, |expression| {
            expression.bend_semitones = 0.6
        });
        render(&mut engine, &fixture, 240);
        let bent = engine.voices()[0].rate_hz();

        let expected = crate::tuning::note_to_rate(59.5, crate::tuning::PitchTable::Hardware);
        assert!(
            (bent - expected).abs() < 1.0,
            "expected {expected} Hz for note 59.5, got {bent}"
        );
        assert!(bent > unbent);
    }

    #[test]
    fn a_terminated_voice_is_reported_once() {
        let mut fixture = Fixture::new();
        fixture.params.release = 1;
        let mut engine = Engine::new(4, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        render(&mut engine, &fixture, 480);
        engine.note_off(None, Some(0), Some(59));
        render(&mut engine, &fixture, 48_000 / 10);

        let mut reported = Vec::new();
        engine.drain_terminated(|key| reported.push(key));
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].note, 59);

        // Draining is destructive: the host must not be told twice.
        let mut again = Vec::new();
        engine.drain_terminated(|key| again.push(key));
        assert!(again.is_empty());
    }

    /// A sine at `frequency`, stored the way the SD card would hold it.
    fn sine_sample(frequency: f32) -> SampleBuffer {
        let rate = 22050.0f32;
        let data: Vec<u8> = (0..(rate * 2.0) as usize)
            .map(|i| {
                let t = i as f32 / rate;
                let value = (2.0 * std::f32::consts::PI * frequency * t).sin() * 0.8;
                ((value * 127.0) + 128.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();
        SampleBuffer::new("sine", data, 22050)
    }

    /// Measure the pitch of rendered audio by running it back through the detector.
    fn measure(rendered: &[f32], sample_rate: f32) -> f32 {
        let peak = rendered.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(peak > 0.01, "nothing was rendered");
        let bytes: Vec<u8> = rendered
            .iter()
            .map(|s| ((s / peak * 120.0) + 128.0).round().clamp(0.0, 255.0) as u8)
            .collect();
        crate::pitch::detect_root(&bytes, sample_rate)
            .expect("rendered audio should have a pitch")
            .frequency
    }

    #[test]
    fn the_output_sounds_the_note_that_was_played() {
        // The whole point of root detection: play a note, hear that note.
        let mut fixture = Fixture::new();
        fixture.sample = sine_sample(220.0);
        let root = fixture
            .sample
            .detected_root()
            .expect("a sine has a pitch")
            .note;
        assert!(
            (root - 57.0).abs() < 0.1,
            "sample root should be A3, got {root}"
        );

        fixture.tuning = Tuning {
            table: crate::tuning::PitchTable::EqualTemperament,
            root_note: root,
            ..Default::default()
        };

        for note in [50u8, 57, 62, 69] {
            let mut engine = Engine::new(4, 48_000.0);
            engine.note_on(&fixture.scene(), key(note), 1.0, Expression::default());
            let (left, _) = render(&mut engine, &fixture, 48_000);

            let measured = measure(&left, 48_000.0);
            let wanted = crate::pitch::frequency_from_note(note as f32);
            let cents = 1200.0 * (measured / wanted).log2();
            assert!(
                cents.abs() < 15.0,
                "note {note}: wanted {wanted:.1} Hz, got {measured:.1} Hz ({cents:+.1} cents off)"
            );
        }
    }

    #[test]
    fn a_quarter_tone_bend_comes_out_a_quarter_tone_higher() {
        let mut fixture = Fixture::new();
        fixture.sample = sine_sample(220.0);
        let root = fixture.sample.detected_root().unwrap().note;
        fixture.tuning = Tuning {
            table: crate::tuning::PitchTable::EqualTemperament,
            root_note: root,
            ..Default::default()
        };

        let render_note = |bend: f32| {
            let mut engine = Engine::new(4, 48_000.0);
            engine.note_on(&fixture.scene(), key(60), 1.0, Expression::default());
            engine.for_each_matching(None, None, None, |expression| {
                expression.bend_semitones = bend
            });
            let (left, _) = render(&mut engine, &fixture, 48_000);
            measure(&left, 48_000.0)
        };

        let plain = render_note(0.0);
        // Half a semitone: a quarter tone.
        let bent = render_note(0.5);
        let cents = 1200.0 * (bent / plain).log2();
        assert!(
            (cents - 50.0).abs() < 10.0,
            "expected a 50 cent rise, got {cents:+.1}"
        );
    }

    #[test]
    fn matching_off_plays_the_sample_at_its_recorded_pitch() {
        // The hardware's behaviour: note 59 plays the file untransposed, whatever pitch it is.
        let mut fixture = Fixture::new();
        fixture.sample = sine_sample(220.0);
        fixture.tuning = Tuning::default();

        let mut engine = Engine::new(4, 48_000.0);
        engine.note_on(&fixture.scene(), key(59), 1.0, Expression::default());
        let (left, _) = render(&mut engine, &fixture, 48_000);

        let measured = measure(&left, 48_000.0);
        let cents = 1200.0 * (measured / 220.0).log2();
        assert!(cents.abs() < 15.0, "expected 220 Hz, got {measured:.1} Hz");
    }

    #[test]
    fn output_stays_within_range_under_full_polyphony() {
        let mut fixture = Fixture::new();
        fixture.params.crush = 127;
        fixture.params.grain = 30;
        let mut engine = Engine::new(16, 48_000.0);
        for note in 48..64 {
            engine.note_on(&fixture.scene(), key(note), 1.0, Expression::default());
        }
        let (left, right) = render(&mut engine, &fixture, 48_000);
        for s in left.iter().chain(right.iter()) {
            assert!(s.is_finite(), "non-finite output");
            assert!(s.abs() < 32.0, "runaway output: {s}");
        }
    }
}
