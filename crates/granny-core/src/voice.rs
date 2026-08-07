//! One playing voice: a read head into the sample with its own grain clock and envelope.
//!
//! The hardware is monophonic, so a "voice" there is just the global state in `SOUND.ino`. Making
//! it polyphonic means giving every note its own copy of that state — position, grain timer,
//! envelope, resolved parameters — while the eight knobs stay global and are re-read every control
//! tick, exactly as `renderTweaking` does on the device.

use crate::curves::{
    grain_ms, shift_bytes, sync_end_ticks, sync_grain_ticks, value_to_sample_rate,
};
use crate::dac::{crush_mask, dac_to_f32, dac_word};
use crate::envelope::{vol_mult_offset, Envelope};
use crate::params::{resolve, Expression, Fidelity, HwParams, ModSlot};
use crate::rng::Xorshift96;
use crate::sample::SampleBuffer;
use crate::tables::{BLOCK_SIZE, NATIVE_RATE, SLICE_COUNT, TABLE_FIRST_NOTE};
use crate::tuning::Tuning;

/// Host transport state, already converted to the 24-ppq clock the firmware counts in.
#[derive(Copy, Clone, Debug, Default)]
pub struct Transport {
    /// Whether the host is rolling. The firmware only honours SYNC while clock is arriving.
    pub playing: bool,
    /// Position in 24-ppq ticks since the start of the timeline.
    pub tick: i64,
}

/// Identity of a voice, as the host knows it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VoiceKey {
    /// The host's voice id, or a fallback derived from channel and note.
    pub voice_id: i32,
    pub channel: u8,
    pub note: u8,
}

/// Everything a voice needs to advance one control tick.
pub struct TickCtx<'a> {
    pub sample: &'a SampleBuffer,
    pub params: &'a HwParams,
    pub fidelity: &'a Fidelity,
    pub tuning: &'a Tuning,
    pub mods: &'a [ModSlot],
    pub transport: Transport,
    pub now_ms: u64,
    pub sample_rate: f32,
}

/// In pitch mode the RATE knob has no effect on the hardware, because the note sets the rate. Here
/// it becomes a transpose, taking its taper from the same curve so the knob still feels the same:
/// the default of 877 is zero transpose, and the ends give -35.9 and +5.0 semitones.
fn rate_transpose_semitones(rate: u16) -> f32 {
    let hz = value_to_sample_rate(rate, false) as f32;
    12.0 * (hz / NATIVE_RATE).log2()
}

fn pan_gains(pan: f32) -> (f32, f32) {
    let x = (pan.clamp(-1.0, 1.0) + 1.0) * 0.5;
    ((1.0 - x).sqrt(), x.sqrt())
}

#[derive(Clone, Debug)]
pub struct Voice {
    pub key: VoiceKey,
    /// Monotonic counter used to pick the oldest voice when stealing.
    pub internal_id: u64,
    pub active: bool,
    /// Whether the key is still physically down, for hold and sustain.
    pub held: bool,
    pub expr: Expression,

    // Resolved once per control tick.
    rate_hz: f32,
    step: f64,
    crush: u16,
    vol_mult: u8,
    vol_offset: u16,
    level: f32,
    pan: f32,
    shift: i32,
    grain_active: bool,
    grain_len_ms: u16,
    grain_ticks: u16,
    end_ticks: u16,
    reverse: bool,

    // Playback position, in file byte offsets.
    pos: f64,
    grain_origin: i64,
    start_pos: i64,
    end_pos: i64,

    // Timers.
    env: Envelope,
    grain_timer_ms: u64,
    last_grain_tick: i64,
    last_end_tick: i64,

    // True once the read head has run off the end of the file, mirroring `WaveRP`'s pause flag.
    paused: bool,

    // Optional grain fade (a cleanup, off by default).
    fade: f32,
    fade_inc: f32,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            key: VoiceKey {
                voice_id: 0,
                channel: 0,
                note: 0,
            },
            internal_id: 0,
            active: false,
            held: false,
            expr: Expression::default(),
            rate_hz: NATIVE_RATE,
            step: 1.0,
            crush: 0,
            vol_mult: 0,
            vol_offset: 0,
            level: 1.0,
            pan: 0.0,
            shift: 0,
            grain_active: false,
            grain_len_ms: 0,
            grain_ticks: 0,
            end_ticks: 0,
            reverse: false,
            pos: 0.0,
            grain_origin: 0,
            start_pos: 0,
            end_pos: 0,
            env: Envelope::default(),
            grain_timer_ms: 0,
            last_grain_tick: i64::MIN,
            last_end_tick: i64::MIN,
            paused: false,
            fade: 1.0,
            fade_inc: 0.0,
        }
    }
}

impl Voice {
    /// Begin a note. Mirrors `playSound` + `loadValuesFromMemmory` + `startEnvelope`.
    pub fn start(
        &mut self,
        key: VoiceKey,
        internal_id: u64,
        velocity: f32,
        expr: Expression,
        ctx: &TickCtx,
        rng: &mut Xorshift96,
    ) {
        self.key = key;
        self.internal_id = internal_id;
        self.active = true;
        self.held = true;
        self.paused = false;
        self.expr = expr;
        self.expr.velocity = velocity;
        self.expr.random = rng.rand(1024) as f32 / 1024.0;

        let r = resolve(ctx.params, ctx.mods, &self.expr);
        let midi_velocity = (velocity * 127.0).round().clamp(0.0, 127.0) as u8;
        self.env.start(
            midi_velocity,
            ctx.params.vel_sensitivity,
            r.attack,
            ctx.now_ms,
        );

        self.grain_timer_ms = ctx.now_ms;
        self.last_grain_tick = i64::MIN;
        self.last_end_tick = i64::MIN;

        self.recompute(ctx, rng);
        self.restart(ctx);

        // The render loop runs before the voice's first control tick, so the envelope has to be
        // evaluated here too. Without this the voice would play at whatever multiplier it was left
        // with — unity gain on a fresh voice — for up to a millisecond, a full-volume burst at the
        // head of every note regardless of the attack setting. The tick also resolves a zero attack
        // to full volume immediately, so a snappy note stays snappy instead of starting silent.
        self.env.tick(ctx.now_ms, r.release, self.reverse);
        self.latch_envelope_gain();
    }

    /// Carry the envelope's attenuation into the multiplier the DAC stage uses.
    fn latch_envelope_gain(&mut self) {
        let (mult, offset) = vol_mult_offset(self.env.attenuation());
        self.vol_mult = mult;
        self.vol_offset = offset;
    }

    /// Retune a sounding voice without retriggering it, for LEGATO.
    pub fn relegato(&mut self, note: u8, ctx: &TickCtx, rng: &mut Xorshift96) {
        self.key.note = note;
        self.recompute(ctx, rng);
    }

    /// The key came up. Whether that starts the release is the engine's call, because hold and the
    /// sustain pedal can defer it.
    pub fn key_up(&mut self) {
        self.held = false;
    }

    pub fn release(&mut self, now_ms: u64) {
        self.env.release(now_ms);
    }

    /// Stop immediately, without a release stage.
    pub fn choke(&mut self) {
        self.env.kill();
        self.active = false;
    }

    pub fn is_releasing(&self) -> bool {
        !self.held
    }

    pub fn rate_hz(&self) -> f32 {
        self.rate_hz
    }

    /// Where the read head currently is, as a fraction of the sample, for the editor.
    pub fn position_fraction(&self, sample: &SampleBuffer) -> f32 {
        sample.position_fraction(self.pos as i64)
    }

    fn end_now(&mut self) {
        self.active = false;
        self.env.kill();
    }

    /// Re-read every parameter, exactly as `renderTweaking` does on each pass of the UI loop.
    fn recompute(&mut self, ctx: &TickCtx, rng: &mut Xorshift96) {
        let r = resolve(ctx.params, ctx.mods, &self.expr);
        let slice_mode = !ctx.params.tuned;

        self.rate_hz = if slice_mode {
            // Untuned: the note picks a slice and the RATE knob sets the pitch.
            value_to_sample_rate(r.rate, false) as f32
        } else {
            let transpose = rate_transpose_semitones(r.rate);
            let offset = self.expr.pitch_offset() + transpose;
            match ctx.tuning.effective_note(self.key.note, offset) {
                Some(note) => ctx.tuning.rate_hz(note),
                // An unmapped key in a Scala keyboard map does not sound.
                None => {
                    self.end_now();
                    return;
                }
            }
        };
        self.step = (self.rate_hz / ctx.sample_rate) as f64;

        self.crush = crush_mask(r.crush);
        self.level = r.level;
        self.pan = r.pan;

        let mut shift = shift_bytes(r.shift, ctx.fidelity.curve_mode);
        if ctx.params.random_shift && rng.coin_flip() {
            shift = -shift;
        }
        self.shift = shift;
        self.grain_active = r.grain != 0;
        self.reverse = shift < 0 && self.grain_active;

        self.grain_len_ms = grain_ms(r.grain, ctx.fidelity.curve_mode);
        self.grain_ticks = sync_grain_ticks(r.grain);
        self.end_ticks = sync_end_ticks(r.end);

        // Start and end, in file byte offsets.
        let granule = ctx.sample.granule(slice_mode);
        let start_index = if slice_mode {
            (self.key.note as i64 - TABLE_FIRST_NOTE as i64).clamp(0, SLICE_COUNT - 1)
        } else {
            r.start as i64
        };
        self.start_pos = start_index * granule;
        self.end_pos = if r.end < 1000 {
            // The firmware refuses to let end collapse onto start.
            (r.end as i64).max(start_index + 10) * granule
        } else {
            ctx.sample.file_size() - BLOCK_SIZE
        };
        if self.end_pos <= self.start_pos {
            self.end_pos = self.start_pos + 1;
        }

        self.fade_inc = if ctx.fidelity.grain_fade_ms > 0.0 {
            1.0 / (ctx.fidelity.grain_fade_ms * ctx.sample_rate / 1000.0).max(1.0)
        } else {
            0.0
        };
    }

    /// One pass of the firmware's main loop for this voice.
    pub fn control_tick(&mut self, ctx: &TickCtx, rng: &mut Xorshift96) {
        if !self.active {
            return;
        }
        if ctx.sample.is_empty() {
            self.end_now();
            return;
        }

        self.recompute(ctx, rng);
        if !self.active {
            return;
        }

        // `updateSound`: a paused player means the read head ran off the end of the file.
        if self.paused {
            self.paused = false;
            if ctx.params.repeat {
                self.restart(ctx);
            } else {
                self.end_now();
                return;
            }
        }

        self.env.tick(
            ctx.now_ms,
            resolve(ctx.params, ctx.mods, &self.expr).release,
            self.reverse,
        );
        if self.env.is_finished() {
            self.end_now();
            return;
        }
        self.latch_envelope_gain();

        let synced = ctx.params.sync && ctx.transport.playing;
        self.render_looping(ctx, synced);
        if !self.active {
            return;
        }
        self.render_granular(ctx, synced);
    }

    /// `renderLooping`.
    fn render_looping(&mut self, ctx: &TickCtx, synced: bool) {
        let pos = self.pos as i64;

        if synced {
            let ticks = self.end_ticks as i64;
            let tick = ctx.transport.tick;
            if ticks > 0 && tick % ticks == 0 && tick != self.last_end_tick {
                self.last_end_tick = tick;
                if ctx.params.repeat {
                    self.restart(ctx);
                } else {
                    self.end_now();
                }
            }
            return;
        }

        if self.reverse {
            if pos <= self.start_pos {
                if ctx.params.repeat {
                    self.grain_origin = self.end_pos;
                    self.do_grain_shift(ctx);
                } else {
                    self.end_now();
                    return;
                }
            }
            // A reversed grain still plays *forwards* from its origin — only the origin marches
            // backwards — so the head walks off the top of the loop and the next grain is what
            // takes it back. Bound that by END, not just by the end of the file: left to run into
            // the tail of the sample the head trips `paused`, and the restart that follows puts it
            // back on the last block and resets the grain clock, so a reversed voice would sit
            // there stuttering instead of marching back through the loop.
            if pos >= self.end_pos || pos >= ctx.sample.file_size() - 600 {
                self.advance_grain(ctx);
            }
        } else if pos >= self.end_pos {
            if ctx.params.repeat {
                self.restart(ctx);
            } else {
                self.end_now();
            }
        }
    }

    /// `renderGranular`.
    fn render_granular(&mut self, ctx: &TickCtx, synced: bool) {
        if !self.grain_active {
            // Keep the origin glued to the head so enabling grains does not jump.
            self.grain_origin = self.pos as i64;
            return;
        }

        if synced {
            let ticks = self.grain_ticks as i64;
            let tick = ctx.transport.tick;
            if ticks > 0 && tick % ticks == 0 && tick != self.last_grain_tick {
                self.last_grain_tick = tick;
                self.advance_grain(ctx);
            }
        } else if ctx.now_ms.saturating_sub(self.grain_timer_ms) >= self.grain_len_ms as u64 {
            self.grain_timer_ms = ctx.now_ms;
            self.advance_grain(ctx);
        }
    }

    /// Move to the next grain. Wrapping the loop is one pass through it, so with REPEAT off that is
    /// where the voice stops — for a reversed voice it is the only place it can, since its head
    /// never reaches the far end of the loop on its own.
    fn advance_grain(&mut self, ctx: &TickCtx) {
        if self.do_grain_shift(ctx) && !ctx.params.repeat {
            self.end_now();
        }
    }

    /// `loadValuesFromMemmory`'s seek: back to the start, or the end when running in reverse.
    fn restart(&mut self, ctx: &TickCtx) {
        let target = if self.reverse {
            self.end_pos
        } else {
            self.start_pos
        };
        self.seek(ctx, target);
        self.grain_timer_ms = ctx.now_ms;
    }

    /// `doGrainShift`. Reports whether the origin ran out of the loop and had to wrap, which is a
    /// reversed voice's equivalent of the read head reaching END.
    fn do_grain_shift(&mut self, ctx: &TickCtx) -> bool {
        self.grain_origin += self.shift as i64;
        let wrapped = if self.shift >= 0 {
            let past = self.grain_origin >= self.end_pos;
            if past {
                self.grain_origin = self.start_pos;
            }
            past
        } else {
            let past = self.grain_origin >= self.end_pos || self.grain_origin <= self.start_pos;
            if past {
                self.grain_origin = self.end_pos;
            }
            past
        };
        let origin = self.grain_origin;
        self.seek(ctx, origin);
        wrapped
    }

    fn seek(&mut self, ctx: &TickCtx, target: i64) {
        // The origin keeps the exact target: on the hardware it is a plain counter and only
        // `WaveRP::seek` rounds, inside the card driver. Storing the rounded position back here
        // would re-floor the accumulator every grain, so any shift smaller than a block could never
        // build up and the knob would look dead over its whole inner range.
        self.grain_origin = target;
        self.pos = ctx.sample.seek_clamp(target, ctx.fidelity.quantize_seeks) as f64;
        self.paused = false;
        if self.fade_inc > 0.0 {
            self.fade = 0.0;
        }
    }

    /// Render into a stereo pair, adding to whatever is already there.
    pub fn render(
        &mut self,
        sample: &SampleBuffer,
        fidelity: &Fidelity,
        left: &mut [f32],
        right: &mut [f32],
    ) {
        if !self.active || self.paused || sample.is_empty() {
            return;
        }
        let (pan_l, pan_r) = pan_gains(self.pan);
        let gain_l = pan_l * self.level;
        let gain_r = pan_r * self.level;

        for i in 0..left.len().min(right.len()) {
            let index = self.pos.floor() as i64;
            if sample.past_end(index) {
                // The DAC ISR stalls here; the next control tick decides whether to loop or stop.
                self.paused = true;
                break;
            }

            let mut value = dac_to_f32(dac_word(
                sample.sample_at(index),
                self.crush,
                self.vol_mult,
                self.vol_offset,
            ));

            if fidelity.interpolate {
                let next = dac_to_f32(dac_word(
                    sample.sample_at(index + 1),
                    self.crush,
                    self.vol_mult,
                    self.vol_offset,
                ));
                let frac = (self.pos - self.pos.floor()) as f32;
                value += (next - value) * frac;
            }

            if self.fade_inc > 0.0 {
                self.fade = (self.fade + self.fade_inc).min(1.0);
                value *= self.fade;
            }

            left[i] += value * gain_l;
            right[i] += value * gain_r;
            self.pos += self.step;
        }
    }
}
