//! MPE channel state.
//!
//! CLAP hosts deliver per-note expression directly, so this only matters for MIDI input. In an MPE
//! zone each sounding note owns a channel, and that channel's pitch bend, channel pressure and
//! CC74 belong to that note alone; the master channel's versions apply to everything. With the zone
//! off it all behaves like plain MIDI and applies globally.

use crate::params::MpeZoneParam;

/// Which voices an incoming channel message should reach.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Every sounding voice.
    All,
    /// Only voices on this channel.
    Channel(u8),
}

/// MIDI CC numbers this layer cares about.
pub const CC_SLIDE: u8 = 74;
pub const CC_SUSTAIN: u8 = 64;
const CC_DATA_ENTRY_MSB: u8 = 6;
const CC_DATA_ENTRY_LSB: u8 = 38;
const CC_RPN_LSB: u8 = 100;
const CC_RPN_MSB: u8 = 101;

const CHANNELS: usize = 16;

#[derive(Clone, Debug)]
pub struct MpeState {
    zone: MpeZoneParam,
    /// Per-channel bend, -1..1.
    bend: [f32; CHANNELS],
    pressure: [f32; CHANNELS],
    slide: [f32; CHANNELS],
    /// In-progress RPN selection per channel.
    rpn: [(u8, u8); CHANNELS],
    /// Pitch bend range in semitones, once RPN 0 has been received.
    pub rpn_bend_range: Option<f32>,
}

impl Default for MpeState {
    fn default() -> Self {
        Self {
            zone: MpeZoneParam::Lower,
            bend: [0.0; CHANNELS],
            pressure: [0.0; CHANNELS],
            slide: [0.0; CHANNELS],
            rpn: [(0x7F, 0x7F); CHANNELS],
            rpn_bend_range: None,
        }
    }
}

impl MpeState {
    pub fn set_zone(&mut self, zone: MpeZoneParam) {
        if self.zone != zone {
            self.zone = zone;
            self.bend = [0.0; CHANNELS];
            self.pressure = [0.0; CHANNELS];
            self.slide = [0.0; CHANNELS];
        }
    }

    /// Whether a channel is the zone's master channel.
    pub fn is_master(&self, channel: u8) -> bool {
        match self.zone {
            MpeZoneParam::Off => false,
            MpeZoneParam::Lower => channel == 0,
            MpeZoneParam::Upper => channel == 15,
        }
    }

    fn master_channel(&self) -> Option<usize> {
        match self.zone {
            MpeZoneParam::Off => None,
            MpeZoneParam::Lower => Some(0),
            MpeZoneParam::Upper => Some(15),
        }
    }

    /// Where a channel message should land. Master channel messages reach every voice.
    fn target(&self, channel: u8) -> Target {
        match self.zone {
            MpeZoneParam::Off => Target::All,
            _ if self.is_master(channel) => Target::All,
            _ => Target::Channel(channel),
        }
    }

    fn master<T: Copy>(&self, values: &[T; CHANNELS], zero: T) -> T {
        self.master_channel().map_or(zero, |c| values[c])
    }

    pub fn set_bend(&mut self, channel: u8, normalized: f32) -> Target {
        // nih-plug reports bend as 0..1 with 0.5 at rest.
        self.bend[channel as usize % CHANNELS] = (normalized - 0.5) * 2.0;
        self.target(channel)
    }

    pub fn set_pressure(&mut self, channel: u8, value: f32) -> Target {
        self.pressure[channel as usize % CHANNELS] = value;
        self.target(channel)
    }

    pub fn set_slide(&mut self, channel: u8, value: f32) -> Target {
        self.slide[channel as usize % CHANNELS] = value;
        self.target(channel)
    }

    /// Effective bend for a voice on a channel: the zone master plus that channel's own bend.
    pub fn bend_for(&self, channel: u8) -> f32 {
        let channel = channel as usize % CHANNELS;
        match self.zone {
            MpeZoneParam::Off => self.bend[channel],
            _ if self.is_master(channel as u8) => self.bend[channel],
            _ => (self.master(&self.bend, 0.0) + self.bend[channel]).clamp(-1.0, 1.0),
        }
    }

    pub fn pressure_for(&self, channel: u8) -> f32 {
        let channel = channel as usize % CHANNELS;
        match self.zone {
            MpeZoneParam::Off => self.pressure[channel],
            _ => self.pressure[channel].max(self.master(&self.pressure, 0.0)),
        }
    }

    pub fn slide_for(&self, channel: u8) -> f32 {
        let channel = channel as usize % CHANNELS;
        match self.zone {
            MpeZoneParam::Off => self.slide[channel],
            _ if self.is_master(channel as u8) => self.slide[channel],
            _ => self.slide[channel],
        }
    }

    /// Feed a control change to the RPN parser. Returns a new bend range when RPN 0 completes.
    ///
    /// `value` is nih-plug's normalised CC value.
    pub fn handle_rpn(&mut self, channel: u8, cc: u8, value: f32) -> Option<f32> {
        let channel = channel as usize % CHANNELS;
        let raw = (value * 127.0).round().clamp(0.0, 127.0) as u8;

        match cc {
            CC_RPN_MSB => {
                self.rpn[channel].0 = raw;
                None
            }
            CC_RPN_LSB => {
                self.rpn[channel].1 = raw;
                None
            }
            CC_DATA_ENTRY_MSB if self.rpn[channel] == (0, 0) => {
                // RPN 0 is pitch bend sensitivity: MSB semitones, LSB cents.
                let range = raw as f32;
                self.rpn_bend_range = Some(range);
                Some(range)
            }
            CC_DATA_ENTRY_LSB if self.rpn[channel] == (0, 0) => {
                let semitones = self.rpn_bend_range.unwrap_or(2.0).trunc();
                let range = semitones + raw as f32 / 100.0;
                self.rpn_bend_range = Some(range);
                Some(range)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_the_zone_off_everything_is_global() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZoneParam::Off);
        assert_eq!(mpe.set_bend(5, 1.0), Target::All);
        assert_eq!(mpe.bend_for(5), 1.0);
    }

    #[test]
    fn member_channel_bend_only_reaches_that_channel() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZoneParam::Lower);
        assert_eq!(mpe.set_bend(3, 1.0), Target::Channel(3));
        assert_eq!(mpe.bend_for(3), 1.0);
        assert_eq!(mpe.bend_for(4), 0.0, "a sibling channel must not move");
    }

    #[test]
    fn master_channel_bend_reaches_everything_and_stacks() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZoneParam::Lower);
        mpe.set_bend(2, 0.75); // member: +0.5
        assert_eq!(mpe.set_bend(0, 0.75), Target::All);
        // Master +0.5 and member +0.5 combine.
        assert_eq!(mpe.bend_for(2), 1.0);
    }

    #[test]
    fn the_upper_zone_puts_the_master_on_channel_sixteen() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZoneParam::Upper);
        assert!(mpe.is_master(15));
        assert!(!mpe.is_master(0));
        assert_eq!(mpe.set_bend(15, 1.0), Target::All);
    }

    #[test]
    fn changing_zone_clears_stale_expression() {
        let mut mpe = MpeState::default();
        mpe.set_bend(3, 1.0);
        mpe.set_zone(MpeZoneParam::Off);
        assert_eq!(mpe.bend_for(3), 0.0);
    }

    #[test]
    fn rpn_zero_sets_the_bend_range() {
        let mut mpe = MpeState::default();
        assert_eq!(mpe.handle_rpn(0, CC_RPN_MSB, 0.0), None);
        assert_eq!(mpe.handle_rpn(0, CC_RPN_LSB, 0.0), None);
        assert_eq!(
            mpe.handle_rpn(0, CC_DATA_ENTRY_MSB, 48.0 / 127.0),
            Some(48.0)
        );
        assert_eq!(mpe.rpn_bend_range, Some(48.0));
    }

    #[test]
    fn other_rpns_are_ignored() {
        let mut mpe = MpeState::default();
        mpe.handle_rpn(0, CC_RPN_MSB, 0.0);
        mpe.handle_rpn(0, CC_RPN_LSB, 1.0 / 127.0); // RPN 0,1 is fine tuning
        assert_eq!(mpe.handle_rpn(0, CC_DATA_ENTRY_MSB, 1.0), None);
        assert_eq!(mpe.rpn_bend_range, None);
    }
}
