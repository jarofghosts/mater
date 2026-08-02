//! The loaded sample, modelled as the file the hardware would have found on its SD card.
//!
//! The firmware addresses audio by *byte offset into the whole file*, header included, so start,
//! end and grain shift are all byte quantities. We keep that model: [`SampleBuffer::file_size`]
//! includes a nominal WAV header, and [`SampleBuffer::sample_at`] maps a file offset back to
//! audio. The consequence — the first 468 samples of any file are unreachable, because
//! `WaveRP::seek` refuses to seek below one SD block — is hardware behaviour, not an off-by-one.

use crate::pitch::{self, Detection};
use crate::tables::{BLOCK_SIZE, GRANULE_COUNT, HEADER_BYTES, NATIVE_RATE, SLICE_COUNT};

/// Number of min/max buckets kept for drawing the waveform.
pub const PEAK_BUCKETS: usize = 2048;

/// 8-bit unsigned mono audio plus a precomputed peak envelope for the editor.
#[derive(Clone, Debug, Default)]
pub struct SampleBuffer {
    pub name: String,
    /// Unsigned 8-bit mono, exactly what the DAC ISR reads off the card.
    data: Vec<u8>,
    /// The rate the file was decoded at, for display only. Playback rate comes from the note.
    pub source_rate: u32,
    peaks: Vec<(u8, u8)>,
    detected_root: Option<Detection>,
}

impl SampleBuffer {
    pub fn new(name: impl Into<String>, data: Vec<u8>, source_rate: u32) -> Self {
        let peaks = compute_peaks(&data);
        // Analysed at the native rate, because that is the rate the buffer plays back at when the
        // engine is not transposing: whatever pitch is heard there is the sample's root.
        let detected_root = pitch::detect_root(&data, NATIVE_RATE);
        Self {
            name: name.into(),
            data,
            source_rate,
            peaks,
            detected_root,
        }
    }

    /// The pitch this sample sounds at when played back untransposed, if it has a clear one.
    pub fn detected_root(&self) -> Option<Detection> {
        self.detected_root
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Min/max pairs for drawing, in the same 0..=255 space as the audio.
    pub fn peaks(&self) -> &[(u8, u8)] {
        &self.peaks
    }

    /// Total size the firmware would see, header included.
    pub fn file_size(&self) -> i64 {
        HEADER_BYTES + self.data.len() as i64
    }

    /// One step of the start/end parameters, in bytes. `slice_mode` divides by 60 instead of 1024.
    pub fn granule(&self, slice_mode: bool) -> i64 {
        let divisor = if slice_mode {
            SLICE_COUNT
        } else {
            GRANULE_COUNT
        };
        (self.file_size() / divisor).max(1)
    }

    /// Read a sample at a file offset. Offsets inside the header read as silence.
    #[inline]
    pub fn sample_at(&self, file_pos: i64) -> u8 {
        let idx = file_pos - HEADER_BYTES;
        if idx < 0 || idx >= self.data.len() as i64 {
            // 0x80 is the unsigned-8-bit midpoint, i.e. silence.
            0x80
        } else {
            self.data[idx as usize]
        }
    }

    /// True once a file offset has run past the end of the audio.
    #[inline]
    pub fn past_end(&self, file_pos: i64) -> bool {
        file_pos - HEADER_BYTES >= self.data.len() as i64
    }

    /// `WaveRP::seek`: floor to an SD block, never seek into metadata, never past the end.
    ///
    /// With `quantize` off the block flooring is skipped, which removes the ~23 ms granularity of
    /// grain start points at 22050 Hz. That is a cleanup, not hardware behaviour.
    pub fn seek_clamp(&self, pos: i64, quantize: bool) -> i64 {
        let mut pos = if quantize {
            pos - pos.rem_euclid(BLOCK_SIZE)
        } else {
            pos
        };
        let floor = if quantize { BLOCK_SIZE } else { HEADER_BYTES };
        if pos <= floor {
            pos = floor;
        }
        let end = self.file_size();
        if pos > end {
            pos = end;
        }
        pos
    }

    /// Fraction of the audio a file offset corresponds to, for the editor's playhead.
    pub fn position_fraction(&self, file_pos: i64) -> f32 {
        if self.data.is_empty() {
            return 0.0;
        }
        ((file_pos - HEADER_BYTES) as f32 / self.data.len() as f32).clamp(0.0, 1.0)
    }
}

fn compute_peaks(data: &[u8]) -> Vec<(u8, u8)> {
    if data.is_empty() {
        return Vec::new();
    }
    let buckets = PEAK_BUCKETS.min(data.len());
    let per_bucket = data.len().div_ceil(buckets);
    data.chunks(per_bucket)
        .map(|chunk| {
            let mut lo = u8::MAX;
            let mut hi = u8::MIN;
            for &s in chunk {
                lo = lo.min(s);
                hi = hi.max(s);
            }
            (lo, hi)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(len: usize) -> SampleBuffer {
        SampleBuffer::new("test", vec![0x80; len], 22050)
    }

    #[test]
    fn file_size_includes_the_header() {
        let s = buffer(1000);
        assert_eq!(s.file_size(), 1044);
    }

    #[test]
    fn seeks_snap_down_to_sd_blocks() {
        let s = buffer(100_000);
        assert_eq!(s.seek_clamp(1023, true), 512);
        assert_eq!(s.seek_clamp(1024, true), 1024);
        assert_eq!(s.seek_clamp(1025, true), 1024);
    }

    #[test]
    fn seeks_never_reach_the_metadata_block() {
        let s = buffer(100_000);
        assert_eq!(s.seek_clamp(0, true), 512);
        assert_eq!(s.seek_clamp(-9999, true), 512);
        // With quantization disabled we can reach the true start of the audio.
        assert_eq!(s.seek_clamp(0, false), HEADER_BYTES);
    }

    #[test]
    fn seeks_clamp_to_the_end_of_the_file() {
        let s = buffer(10_000);
        assert_eq!(s.seek_clamp(999_999, true), s.file_size());
    }

    #[test]
    fn header_offsets_read_as_silence() {
        let mut data = vec![0u8; 100];
        data[0] = 200;
        let s = SampleBuffer::new("t", data, 22050);
        assert_eq!(s.sample_at(0), 0x80);
        assert_eq!(s.sample_at(HEADER_BYTES), 200);
        assert_eq!(s.sample_at(HEADER_BYTES + 1), 0);
    }

    #[test]
    fn granules_divide_the_file_the_way_the_firmware_does() {
        let s = buffer(102_400);
        assert_eq!(s.granule(false), s.file_size() / 1024);
        assert_eq!(s.granule(true), s.file_size() / 60);
    }

    #[test]
    fn granule_never_collapses_to_zero_on_tiny_files() {
        let s = buffer(4);
        assert!(s.granule(false) >= 1);
    }

    #[test]
    fn peaks_track_the_extremes() {
        let mut data = vec![0x80u8; 10_000];
        data[5_000] = 255;
        data[5_001] = 0;
        let s = SampleBuffer::new("t", data, 22050);
        assert!(s.peaks().iter().any(|&(lo, _)| lo == 0));
        assert!(s.peaks().iter().any(|&(_, hi)| hi == 255));
    }
}
