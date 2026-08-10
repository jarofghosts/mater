//! MPE channel state.
//!
//! Hosts that deliver per-note expression directly (CLAP note expressions) bypass this; it is what
//! plain MIDI input needs. In an MPE
//! zone each sounding note owns a channel, and that channel's pitch bend, channel pressure and
//! CC74 belong to that note alone; the master channel's versions apply to everything. With the zone
//! off it all behaves like plain MIDI and applies globally.

/// Which MPE zone is in force.
///
/// `Off` means plain MIDI: bend, pressure and CC74 apply to every voice. A wrapper with its own
/// parameter type maps onto this rather than duplicating the state machine below.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum MpeZone {
    Off,
    /// Master on channel 1, members on 2-16.
    #[default]
    Lower,
    /// Master on channel 16, members on 15-1.
    Upper,
}

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

/// MPE's member-channel default: a member channel bends by ±48 semitones.
pub const DEFAULT_MEMBER_BEND_RANGE: f32 = 48.0;
/// The ordinary MIDI default, which MPE keeps for the master channel: ±2 semitones.
///
/// This is also the right range with the zone off, because then the input is plain MIDI.
pub const DEFAULT_MASTER_BEND_RANGE: f32 = 2.0;

/// Floor on a bend range, so a range of zero cannot silently disable bend entirely.
const MIN_RANGE: f32 = 0.01;

#[derive(Clone, Debug)]
pub struct MpeState {
    zone: MpeZone,
    /// Per-channel bend, -1..1.
    bend: [f32; CHANNELS],
    pressure: [f32; CHANNELS],
    slide: [f32; CHANNELS],
    /// In-progress RPN selection per channel.
    rpn: [(u8, u8); CHANNELS],
    /// Bend range in semitones for member channels, from the parameter.
    member_range: f32,
    /// Bend range in semitones for the master channel, and for everything with the zone off.
    master_range: f32,
    /// Ranges reported by RPN 0, kept apart because the two channel classes are configured
    /// independently — a controller announcing ±2 on its master says nothing about its members.
    rpn_member_range: Option<f32>,
    rpn_master_range: Option<f32>,
    follow_rpn: bool,
}

impl Default for MpeState {
    fn default() -> Self {
        Self {
            zone: MpeZone::Lower,
            bend: [0.0; CHANNELS],
            pressure: [0.0; CHANNELS],
            slide: [0.0; CHANNELS],
            rpn: [(0x7F, 0x7F); CHANNELS],
            member_range: DEFAULT_MEMBER_BEND_RANGE,
            master_range: DEFAULT_MASTER_BEND_RANGE,
            rpn_member_range: None,
            rpn_master_range: None,
            follow_rpn: true,
        }
    }
}

impl MpeState {
    pub fn set_zone(&mut self, zone: MpeZone) {
        if self.zone != zone {
            self.zone = zone;
            self.bend = [0.0; CHANNELS];
            self.pressure = [0.0; CHANNELS];
            self.slide = [0.0; CHANNELS];
            // Which channels are master has changed, so anything RPN said before was about a
            // different set of channels.
            self.rpn_member_range = None;
            self.rpn_master_range = None;
        }
    }

    /// Set the bend ranges the parameters ask for, and whether RPN 0 may override them.
    pub fn set_ranges(&mut self, member: f32, master: f32, follow_rpn: bool) {
        self.member_range = member.max(MIN_RANGE);
        self.master_range = master.max(MIN_RANGE);
        self.follow_rpn = follow_rpn;
    }

    /// Whether a channel's bend is measured against the master range.
    ///
    /// With the zone off there is no master channel — but there is no MPE either, so the input is
    /// plain MIDI and the ordinary bend range is the one that applies.
    fn uses_master_range(&self, channel: u8) -> bool {
        self.zone == MpeZone::Off || self.is_master(channel)
    }

    /// Bend range in semitones for member channels: MPE's wide per-note range.
    pub fn member_bend_range(&self) -> f32 {
        match (self.follow_rpn, self.rpn_member_range) {
            (true, Some(range)) => range.max(MIN_RANGE),
            _ => self.member_range,
        }
    }

    /// Bend range in semitones for the master channel, and for plain MIDI with the zone off.
    pub fn master_bend_range(&self) -> f32 {
        match (self.follow_rpn, self.rpn_master_range) {
            (true, Some(range)) => range.max(MIN_RANGE),
            _ => self.master_range,
        }
    }

    /// Whether a channel is the zone's master channel.
    pub fn is_master(&self, channel: u8) -> bool {
        match self.zone {
            MpeZone::Off => false,
            MpeZone::Lower => channel == 0,
            MpeZone::Upper => channel == 15,
        }
    }

    fn master_channel(&self) -> Option<usize> {
        match self.zone {
            MpeZone::Off => None,
            MpeZone::Lower => Some(0),
            MpeZone::Upper => Some(15),
        }
    }

    /// Where a channel message should land. Master channel messages reach every voice.
    fn target(&self, channel: u8) -> Target {
        match self.zone {
            MpeZone::Off => Target::All,
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

    /// Raw bend for a channel, -1..1, as the controller sent it. The mod matrix source.
    pub fn bend_for(&self, channel: u8) -> f32 {
        let index = channel as usize % CHANNELS;
        if self.uses_master_range(index as u8) {
            self.bend[index]
        } else {
            (self.master(&self.bend, 0.0) + self.bend[index]).clamp(-1.0, 1.0)
        }
    }

    /// What a voice's bend is worth in semitones: the zone master plus that channel's own bend,
    /// each against its own range.
    ///
    /// The two channel classes do not share a bend range, and resolving them together is the whole
    /// point. Getting it wrong is not subtle: a quarter tone sent as a ±2 semitone bend, read as if
    /// it were MPE's ±48, comes out exactly an octave away.
    pub fn bend_semitones_for(&self, channel: u8) -> f32 {
        let index = channel as usize % CHANNELS;
        if self.uses_master_range(index as u8) {
            self.bend[index] * self.master_bend_range()
        } else {
            self.master(&self.bend, 0.0) * self.master_bend_range()
                + self.bend[index] * self.member_bend_range()
        }
    }

    pub fn pressure_for(&self, channel: u8) -> f32 {
        let channel = channel as usize % CHANNELS;
        match self.zone {
            MpeZone::Off => self.pressure[channel],
            _ => self.pressure[channel].max(self.master(&self.pressure, 0.0)),
        }
    }

    pub fn slide_for(&self, channel: u8) -> f32 {
        let channel = channel as usize % CHANNELS;
        match self.zone {
            MpeZone::Off => self.slide[channel],
            _ if self.is_master(channel as u8) => self.slide[channel],
            _ => self.slide[channel],
        }
    }

    /// The RPN-reported range for whichever class a channel belongs to.
    fn rpn_range(&self, channel: u8) -> Option<f32> {
        if self.uses_master_range(channel) {
            self.rpn_master_range
        } else {
            self.rpn_member_range
        }
    }

    fn record_rpn_range(&mut self, channel: u8, range: f32) -> f32 {
        if self.uses_master_range(channel) {
            self.rpn_master_range = Some(range);
        } else {
            self.rpn_member_range = Some(range);
        }
        range
    }

    /// Feed a control change to the RPN parser. Returns a new bend range when RPN 0 completes.
    ///
    /// The range applies to the class of channel it arrived on, not to everything: MPE configures
    /// its master and its members separately, and they routinely differ by a factor of 24.
    ///
    /// `value` is nih-plug's normalised CC value.
    pub fn handle_rpn(&mut self, channel: u8, cc: u8, value: f32) -> Option<f32> {
        let index = channel as usize % CHANNELS;
        let raw = (value * 127.0).round().clamp(0.0, 127.0) as u8;

        match cc {
            CC_RPN_MSB => {
                self.rpn[index].0 = raw;
                None
            }
            CC_RPN_LSB => {
                self.rpn[index].1 = raw;
                None
            }
            CC_DATA_ENTRY_MSB if self.rpn[index] == (0, 0) => {
                // RPN 0 is pitch bend sensitivity: MSB semitones, LSB cents.
                Some(self.record_rpn_range(channel, raw as f32))
            }
            CC_DATA_ENTRY_LSB if self.rpn[index] == (0, 0) => {
                let semitones = self.rpn_range(channel).unwrap_or(2.0).trunc();
                Some(self.record_rpn_range(channel, semitones + raw as f32 / 100.0))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quarter tone up — half a semitone — from a controller with the ordinary ±2 range, as
    /// nih-plug reports it: 0..1 with 0.5 at rest.
    const QUARTER_TONE_BEND: f32 = 0.5 + 0.5 * (0.5 / 2.0);

    #[test]
    fn with_the_zone_off_everything_is_global() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Off);
        assert_eq!(mpe.set_bend(5, 1.0), Target::All);
        // Plain MIDI, so a full-scale bend is the ordinary two semitones.
        assert!((mpe.bend_semitones_for(5) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn a_quarter_tone_from_a_plain_midi_controller_is_a_quarter_tone() {
        // The bug this guards: read against MPE's ±48 instead of the ordinary ±2, a quarter tone
        // lands exactly an octave away, which is precisely what makes it easy to miss.
        for zone in [MpeZone::Off, MpeZone::Lower] {
            let mut mpe = MpeState::default();
            mpe.set_zone(zone);
            // Channel 0 is the master channel of the lower zone, and the only channel a
            // single-channel controller ever uses.
            mpe.set_bend(0, QUARTER_TONE_BEND);
            let bent = mpe.bend_semitones_for(0);
            assert!(
                (bent - 0.5).abs() < 1e-3,
                "{zone:?}: expected half a semitone, got {bent}"
            );
        }
    }

    #[test]
    fn a_member_channel_still_gets_the_full_mpe_range() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Lower);
        mpe.set_bend(3, 1.0);
        assert!((mpe.bend_semitones_for(3) - 48.0).abs() < 1e-3);
    }

    #[test]
    fn member_channel_bend_only_reaches_that_channel() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Lower);
        assert_eq!(mpe.set_bend(3, 1.0), Target::Channel(3));
        assert_eq!(mpe.bend_for(3), 1.0);
        assert_eq!(mpe.bend_for(4), 0.0, "a sibling channel must not move");
    }

    #[test]
    fn master_channel_bend_reaches_everything_and_stacks() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Lower);
        mpe.set_bend(2, 0.75); // member: +0.5 of 48 semitones
        assert_eq!(mpe.set_bend(0, 0.75), Target::All);
        // The two stack in semitones, not in normalised units: half of 48 from the member, half of
        // the master's own 2 from the master.
        assert!((mpe.bend_semitones_for(2) - 25.0).abs() < 1e-3);
    }

    #[test]
    fn the_upper_zone_puts_the_master_on_channel_sixteen() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Upper);
        assert!(mpe.is_master(15));
        assert!(!mpe.is_master(0));
        assert_eq!(mpe.set_bend(15, 1.0), Target::All);
    }

    #[test]
    fn changing_zone_clears_stale_expression() {
        let mut mpe = MpeState::default();
        mpe.set_bend(3, 1.0);
        mpe.set_zone(MpeZone::Off);
        assert_eq!(mpe.bend_for(3), 0.0);
    }

    #[test]
    fn rpn_zero_sets_the_bend_range() {
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Lower);
        assert_eq!(mpe.handle_rpn(1, CC_RPN_MSB, 0.0), None);
        assert_eq!(mpe.handle_rpn(1, CC_RPN_LSB, 0.0), None);
        assert_eq!(
            mpe.handle_rpn(1, CC_DATA_ENTRY_MSB, 48.0 / 127.0),
            Some(48.0)
        );
        assert_eq!(mpe.member_bend_range(), 48.0);
    }

    #[test]
    fn rpn_applies_to_the_channel_class_it_arrived_on() {
        // A controller announcing ±2 on its master must not be taken to mean its members are ±2
        // too; that is the same factor-of-24 error by another route.
        let mut mpe = MpeState::default();
        mpe.set_zone(MpeZone::Lower);
        mpe.handle_rpn(0, CC_RPN_MSB, 0.0);
        mpe.handle_rpn(0, CC_RPN_LSB, 0.0);
        assert_eq!(mpe.handle_rpn(0, CC_DATA_ENTRY_MSB, 2.0 / 127.0), Some(2.0));
        assert_eq!(mpe.master_bend_range(), 2.0);
        assert_eq!(mpe.member_bend_range(), DEFAULT_MEMBER_BEND_RANGE);
    }

    #[test]
    fn rpn_is_ignored_when_not_following_it() {
        let mut mpe = MpeState::default();
        mpe.set_ranges(24.0, 2.0, false);
        mpe.handle_rpn(1, CC_RPN_MSB, 0.0);
        mpe.handle_rpn(1, CC_RPN_LSB, 0.0);
        mpe.handle_rpn(1, CC_DATA_ENTRY_MSB, 48.0 / 127.0);
        assert_eq!(mpe.member_bend_range(), 24.0);
    }

    #[test]
    fn other_rpns_are_ignored() {
        let mut mpe = MpeState::default();
        mpe.handle_rpn(1, CC_RPN_MSB, 0.0);
        mpe.handle_rpn(1, CC_RPN_LSB, 1.0 / 127.0); // RPN 0,1 is fine tuning
        assert_eq!(mpe.handle_rpn(1, CC_DATA_ENTRY_MSB, 1.0), None);
        assert_eq!(mpe.member_bend_range(), DEFAULT_MEMBER_BEND_RANGE);
    }
}
