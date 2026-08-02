//! Constant tables lifted from the microGranny 2.5 firmware.
//!
//! Every table here is transcribed from `bastl-instruments/bastlMicroGranny`
//! (`examples/microGranny2_5/*.ino` and `WaveRP.cpp`). The values are the sound; do not "clean
//! them up".

/// MIDI note -> DAC sample rate, indexed by `note - 23`.
///
/// From `MIDI.ino`. Index 36 (MIDI note 59, B3) is 22050 Hz, the native rate of the stock sample
/// bank. The table is a rounded approximation of equal temperament, not exact ET, and the top
/// entries are clamped on hardware because the AVR cannot clock the DAC any faster.
pub const NOTE_SAMPLE_RATE: [u16; 49] = [
    2772, 2929, 3103, 3281, 3500, 3679, 3910, 4146, 4392, 4660, 4924, 5231, 5528, 5863, 6221, 6579,
    6960, 7355, 7784, 8278, 8786, 9333, 9847, 10420, 11023, 11662, 12402, 13119, 13898, 14706,
    15606, 16491, 17550, 18555, 19677, 20857, 22050, 23420, 24807, 26197, 27815, 29480, 29480,
    29480, 29480, 29480, 29480, 29480, 29480,
];

/// MIDI note that plays the sample back at its native rate (index 36 of [`NOTE_SAMPLE_RATE`]).
pub const NATIVE_NOTE: f32 = 59.0;
/// Sample rate at [`NATIVE_NOTE`].
pub const NATIVE_RATE: f32 = 22050.0;
/// Lowest note present in [`NOTE_SAMPLE_RATE`].
pub const TABLE_FIRST_NOTE: i32 = 23;
/// Highest note before the hardware table flattens out (index 41).
pub const TABLE_LAST_NOTE: i32 = 64;

/// Attenuation in dB -> 8.8 fixed point multiplier, from `WaveRP.cpp`.
///
/// Index 0 is a sentinel: `volMult == 0` means "bypass the multiply entirely", i.e. unity gain.
pub const DB_TO_MULT: [u8; 38] = [
    0, 228, 203, 181, 162, 144, 128, 114, 102, 91, 81, 72, 64, 57, 51, 46, 41, 36, 32, 29, 26, 23,
    20, 18, 16, 14, 13, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
];

/// Musically useful lengths in 24-ppq MIDI clock ticks, from `SOUND.ino`.
///
/// 24 ticks = one quarter note. The final 24000 is the firmware's "effectively never" value.
pub const USEFUL_LENGTHS: [u16; 17] = [
    0, 1, 2, 3, 6, 12, 24, 48, 72, 96, 144, 192, 288, 384, 576, 768, 24000,
];

/// Grain size curve breakpoints (`granSizeMap` in `UI.ino`), input side: parameter << 1.
pub const GRAIN_MAP_IN: [u16; 8] = [0, 5, 10, 80, 127, 160, 220, 255];
/// Grain size curve breakpoints, output side: milliseconds.
pub const GRAIN_MAP_OUT: [u16; 8] = [0, 25, 25, 100, 400, 1000, 2000, 4000];

/// Shift speed curve breakpoints (`shiftSpeedMap` in `UI.ino`), input side: raw parameter.
pub const SHIFT_MAP_IN: [u16; 10] = [0, 30, 60, 85, 110, 146, 180, 195, 220, 255];
/// Shift speed curve breakpoints, output side. [`SHIFT_CENTRE`] is subtracted to make it bipolar.
pub const SHIFT_MAP_OUT: [u16; 10] = [
    0, 5000, 12000, 15000, 16000, 16000, 17000, 20000, 27000, 32000,
];
/// Offset subtracted from the shift curve output to centre it on zero.
pub const SHIFT_CENTRE: i32 = 16000;

/// SD block size. Every seek on hardware is floored to a multiple of this, which is audible.
pub const BLOCK_SIZE: i64 = 512;

/// Size of the WAV header the firmware's byte positions include but never play.
///
/// Positions on hardware are offsets into the whole file, so a 44-byte canonical WAV header sits in
/// front of the audio, and `WaveRP::seek` additionally refuses to seek below [`BLOCK_SIZE`] so it
/// never plays metadata. Together that means the first 468 samples of any file are unreachable.
pub const HEADER_BYTES: i64 = 44;

/// Full-scale value of the 12-bit DAC the hardware writes to.
pub const DAC_MAX: u16 = 4095;
/// DAC code corresponding to silence (an 8-bit unsigned sample of 0x80, shifted left by 4).
pub const DAC_MID: u16 = 2048;

/// Number of slices the sample is divided into in slice ("untuned") note mode.
pub const SLICE_COUNT: i64 = 60;
/// Number of steps the start/end parameters divide the sample into in pitch mode.
pub const GRANULE_COUNT: i64 = 1024;
