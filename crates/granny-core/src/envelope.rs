//! The firmware's amplitude envelope, ported from `startEnvelope`/`renderEnvelope` in `SOUND.ino`.
//!
//! It is not an exponential or linear ramp: it is a counter stepping through the 38-entry
//! [`DB_TO_MULT`] attenuation table, advanced from `millis()`. The attack and release parameters
//! are the *interval between steps in milliseconds*, and below 30 ms the counter moves three steps
//! at a time instead of one. Velocity sets the sustain attenuation, not a multiplier.

use crate::tables::DB_TO_MULT;

/// Attenuation index at which the envelope is considered silent and the voice stops.
const SILENT: i16 = 36;
/// Attenuation index the envelope starts from at note-on.
const START: i16 = 37;
/// Below this step interval the counter moves three indices per step instead of one.
const FAST_STEP_THRESHOLD: u16 = 30;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EnvPhase {
    Attack,
    Sustain,
    Release,
    Finished,
}

/// One voice's amplitude envelope.
///
/// `level` is an *attenuation* in dB-ish table steps: 0 is full volume, 37 is silence.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub phase: EnvPhase,
    level: i16,
    vel_atten: i16,
    /// Latched at note-on, exactly like the firmware's `startEnvelope(velocity, attackInterval)`.
    attack: u16,
    last_step_ms: u64,
}

impl Default for Envelope {
    fn default() -> Self {
        Self {
            phase: EnvPhase::Finished,
            level: START,
            vel_atten: 0,
            attack: 0,
            last_step_ms: 0,
        }
    }
}

impl Envelope {
    /// `startEnvelope`. `velocity` is the raw 7-bit MIDI value.
    pub fn start(&mut self, velocity: u8, attack: u16, now_ms: u64) {
        self.phase = EnvPhase::Attack;
        self.level = START;
        // `velocity = 31 - (_velocity >> 2)`: full velocity means zero attenuation.
        self.vel_atten = 31 - (velocity >> 2) as i16;
        self.attack = attack;
        self.last_step_ms = now_ms;
    }

    /// `stopEnvelope`: begin the release from wherever the envelope currently sits.
    pub fn release(&mut self, now_ms: u64) {
        if self.phase != EnvPhase::Release && self.phase != EnvPhase::Finished {
            self.phase = EnvPhase::Release;
            self.last_step_ms = now_ms;
        }
    }

    /// Cut the voice immediately, without a release stage.
    pub fn kill(&mut self) {
        self.phase = EnvPhase::Finished;
        self.level = START;
    }

    pub fn is_finished(&self) -> bool {
        self.phase == EnvPhase::Finished
    }

    /// Current attenuation index, clamped into the table's range.
    pub fn attenuation(&self) -> u8 {
        self.level.clamp(0, START) as u8
    }

    /// `renderEnvelope`, called once per control tick.
    ///
    /// `release` is read live because the firmware's `renderTweaking` keeps updating
    /// `releaseInterval` while the knob moves. `reverse` reproduces the 2.5 special case where a
    /// zero attack is forced to 1 ms so reversed grains still fade in.
    pub fn tick(&mut self, now_ms: u64, release: u16, reverse: bool) {
        match self.phase {
            EnvPhase::Attack => {
                if self.attack == 0 {
                    if reverse {
                        self.attack = 1;
                    } else {
                        // The firmware momentarily snaps to full volume here before the sustain
                        // stage pulls it back to the velocity attenuation on the next tick.
                        self.level = 0;
                        self.phase = EnvPhase::Sustain;
                    }
                } else if now_ms.saturating_sub(self.last_step_ms) >= self.attack as u64 {
                    self.last_step_ms = now_ms;
                    self.level -= if self.attack < FAST_STEP_THRESHOLD {
                        3
                    } else {
                        1
                    };
                    if self.level <= self.vel_atten {
                        self.level = 0;
                        self.phase = EnvPhase::Sustain;
                    }
                }
            }
            EnvPhase::Sustain => {
                self.level = self.vel_atten;
            }
            EnvPhase::Release => {
                if release == 0 {
                    self.level = SILENT;
                    self.phase = EnvPhase::Finished;
                } else if now_ms.saturating_sub(self.last_step_ms) >= release as u64 {
                    self.last_step_ms = now_ms;
                    self.level += if release < FAST_STEP_THRESHOLD { 3 } else { 1 };
                    if self.level >= SILENT {
                        self.level = SILENT;
                        self.phase = EnvPhase::Finished;
                    }
                }
            }
            EnvPhase::Finished => {}
        }
    }
}

/// `WaveRP::setVolume`: attenuation index -> (multiplier, offset).
///
/// A multiplier of zero is the firmware's "skip the multiply" sentinel, i.e. unity gain. The offset
/// keeps the attenuated signal centred on the DAC midpoint rather than collapsing toward zero.
pub fn vol_mult_offset(db: u8) -> (u8, u16) {
    let mult = if db > 37 { 1 } else { DB_TO_MULT[db as usize] };
    (mult, 2048u16.wrapping_sub(8 * mult as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_gain_is_a_bypass() {
        let (mult, _) = vol_mult_offset(0);
        assert_eq!(mult, 0, "index 0 must signal 'skip the multiply'");
    }

    #[test]
    fn attenuation_collapses_toward_the_dac_midpoint() {
        let (mult, offset) = vol_mult_offset(1);
        assert_eq!((mult, offset), (228, 224));
        // A full-scale-negative sample attenuated by 1 dB stays centred on 2048.
        let quiet = ((mult as u32 * 2048) >> 8) as u16 + offset;
        assert_eq!(quiet, 2048);

        // At the release threshold a full-scale sample is 15 counts off the midpoint, about
        // -42 dBFS. That residual is why the firmware stops the voice here rather than at 37.
        let (mult, offset) = vol_mult_offset(36);
        let quiet = ((mult as u32 * 4095) >> 8) as u16 + offset;
        assert_eq!(quiet, 2063);
        assert!((quiet as i32 - 2048).unsigned_abs() < 4095 / 100);
    }

    #[test]
    fn zero_attack_reaches_sustain_on_the_first_tick() {
        let mut env = Envelope::default();
        env.start(127, 0, 0);
        env.tick(0, 0, false);
        assert_eq!(env.phase, EnvPhase::Sustain);
        assert_eq!(env.attenuation(), 0);
    }

    #[test]
    fn zero_attack_is_forced_to_one_ms_for_reversed_grains() {
        let mut env = Envelope::default();
        env.start(127, 0, 0);
        env.tick(0, 0, true);
        assert_eq!(env.phase, EnvPhase::Attack);
        assert_eq!(env.attack, 1);
    }

    #[test]
    fn long_attack_takes_roughly_four_and_a_half_seconds() {
        let mut env = Envelope::default();
        env.start(127, 127, 0);
        let mut ms = 0u64;
        while env.phase == EnvPhase::Attack && ms < 20_000 {
            ms += 1;
            env.tick(ms, 0, false);
        }
        // 37 single steps at 127 ms each.
        assert_eq!(env.phase, EnvPhase::Sustain);
        assert!((4_600..=4_800).contains(&ms), "attack took {ms} ms");
    }

    #[test]
    fn short_attack_uses_the_three_step_path() {
        let mut env = Envelope::default();
        env.start(127, 29, 0);
        let mut ms = 0u64;
        while env.phase == EnvPhase::Attack && ms < 20_000 {
            ms += 1;
            env.tick(ms, 0, false);
        }
        // 13 triple steps at 29 ms rather than 37 single ones.
        assert!((350..=400).contains(&ms), "attack took {ms} ms");
    }

    #[test]
    fn velocity_sets_the_sustain_attenuation() {
        let mut env = Envelope::default();
        env.start(0, 0, 0);
        env.tick(0, 0, false);
        env.tick(1, 0, false);
        assert_eq!(env.attenuation(), 31, "silent-ish at velocity 0");

        let mut env = Envelope::default();
        env.start(127, 0, 0);
        env.tick(0, 0, false);
        env.tick(1, 0, false);
        assert_eq!(env.attenuation(), 0, "full volume at velocity 127");
    }

    #[test]
    fn zero_release_finishes_immediately() {
        let mut env = Envelope::default();
        env.start(127, 0, 0);
        env.tick(0, 0, false);
        env.release(1);
        env.tick(1, 0, false);
        assert!(env.is_finished());
    }

    #[test]
    fn release_runs_to_silence() {
        let mut env = Envelope::default();
        env.start(127, 0, 0);
        env.tick(0, 100, false);
        env.tick(1, 100, false);
        env.release(1);
        let mut ms = 1u64;
        while !env.is_finished() && ms < 60_000 {
            ms += 1;
            env.tick(ms, 100, false);
        }
        assert!(env.is_finished());
        assert!((3_500..=3_800).contains(&ms), "release took {ms} ms");
    }
}
