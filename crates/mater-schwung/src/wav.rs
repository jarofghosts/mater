//! A WAV reader, and the same conversion to 8-bit the CLAP build's loader does.
//!
//! The CLAP plugin decodes with symphonia, which covers wav, aiff, flac, mp3, ogg and the mp4
//! family. Phase 1 wants a cross-compile with nothing in it, so this reads uncompressed WAV only
//! and leaves the rest to phase 3. The arithmetic after decoding — downmix, resample, normalise,
//! `to_unsigned_8bit` — deliberately mirrors `mater-plugin/src/loader.rs` so a sample sounds the
//! same on the Move as it does in a host.

use std::path::Path;

/// The rate the engine calls note 59. Resampling to it is what makes note 59 play a file back at
/// its original speed.
const NATIVE_RATE: u32 = 22050;

pub struct Decoded {
    pub data: Vec<u8>,
    pub source_rate: u32,
}

pub struct LoadOptions {
    pub resample: bool,
    pub normalize: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            resample: true,
            normalize: true,
        }
    }
}

pub fn load(path: &Path, options: &LoadOptions) -> Result<Decoded, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (mut mono, source_rate) = decode(&bytes)?;

    if mono.is_empty() {
        return Err("no audio in file".into());
    }

    if options.resample && source_rate != NATIVE_RATE {
        mono = resample_linear(&mono, source_rate, NATIVE_RATE);
    }
    if options.normalize {
        normalize(&mut mono);
    }

    Ok(Decoded {
        data: mono.iter().copied().map(to_unsigned_8bit).collect(),
        source_rate: if options.resample {
            NATIVE_RATE
        } else {
            source_rate
        },
    })
}

/// Walk the RIFF chunks for `fmt ` and `data`, and return interleaved-to-mono f32.
fn decode(bytes: &[u8]) -> Result<(Vec<f32>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }

    let mut format: Option<Format> = None;
    let mut offset = 12;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body = offset + 8;
        let end = body.saturating_add(size).min(bytes.len());

        match id {
            b"fmt " => format = Some(parse_fmt(&bytes[body..end])?),
            b"data" => {
                let format = format.ok_or("data chunk before fmt chunk")?;
                return Ok((to_mono(&bytes[body..end], &format)?, format.sample_rate));
            }
            _ => {}
        }

        // Chunks are word-aligned, so an odd size is followed by a pad byte.
        offset = body + size + (size & 1);
    }

    Err("no data chunk".into())
}

struct Format {
    /// 1 = integer PCM, 3 = IEEE float. WAVE_FORMAT_EXTENSIBLE (0xFFFE) is resolved to one of
    /// those by its sub-format GUID, whose first two bytes carry the real tag.
    tag: u16,
    channels: u16,
    sample_rate: u32,
    bits: u16,
}

fn parse_fmt(chunk: &[u8]) -> Result<Format, String> {
    if chunk.len() < 16 {
        return Err("fmt chunk too short".into());
    }

    let mut tag = u16::from_le_bytes(chunk[0..2].try_into().unwrap());
    if tag == 0xFFFE {
        if chunk.len() < 26 {
            return Err("extensible fmt chunk without a sub-format".into());
        }
        tag = u16::from_le_bytes(chunk[24..26].try_into().unwrap());
    }

    let format = Format {
        tag,
        channels: u16::from_le_bytes(chunk[2..4].try_into().unwrap()),
        sample_rate: u32::from_le_bytes(chunk[4..8].try_into().unwrap()),
        bits: u16::from_le_bytes(chunk[14..16].try_into().unwrap()),
    };

    if format.channels == 0 || format.sample_rate == 0 {
        return Err("fmt chunk declares no channels or no rate".into());
    }
    Ok(format)
}

fn to_mono(data: &[u8], format: &Format) -> Result<Vec<f32>, String> {
    let channels = format.channels as usize;
    let bytes_per_sample = match (format.tag, format.bits) {
        (1, 8) | (1, 16) | (1, 24) | (1, 32) => format.bits as usize / 8,
        (3, 32) => 4,
        (3, 64) => 8,
        (tag, bits) => return Err(format!("unsupported WAV format (tag {tag}, {bits} bits)")),
    };

    let stride = bytes_per_sample * channels;
    if stride == 0 {
        return Err("zero-width frames".into());
    }

    let frames = data.len() / stride;
    let mut mono = Vec::with_capacity(frames);

    for frame in 0..frames {
        let base = frame * stride;
        let mut sum = 0.0f32;
        for channel in 0..channels {
            let at = base + channel * bytes_per_sample;
            sum += read_sample(&data[at..at + bytes_per_sample], format.tag);
        }
        mono.push(sum / channels as f32);
    }

    Ok(mono)
}

fn read_sample(bytes: &[u8], tag: u16) -> f32 {
    match (tag, bytes.len()) {
        // 8-bit PCM is the one integer width WAV stores unsigned.
        (1, 1) => (bytes[0] as f32 - 128.0) / 128.0,
        (1, 2) => i16::from_le_bytes(bytes.try_into().unwrap()) as f32 / 32768.0,
        (1, 3) => {
            let raw = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
            // Sign-extend the 24-bit value into the top byte.
            let signed = (raw << 8) >> 8;
            signed as f32 / 8_388_608.0
        }
        (1, 4) => i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0,
        (3, 4) => f32::from_le_bytes(bytes.try_into().unwrap()),
        (3, 8) => f64::from_le_bytes(bytes.try_into().unwrap()) as f32,
        _ => 0.0,
    }
}

fn to_unsigned_8bit(sample: f32) -> u8 {
    ((sample.clamp(-1.0, 1.0) * 127.0) + 128.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }

    let ratio = to as f64 / from as f64;
    let len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
    let mut output = Vec::with_capacity(len);

    for i in 0..len {
        let position = i as f64 / ratio;
        let index = position.floor() as usize;
        let fraction = (position - index as f64) as f32;
        let a = input.get(index).copied().unwrap_or(0.0);
        let b = input.get(index + 1).copied().unwrap_or(a);
        output.push(a + (b - a) * fraction);
    }

    output
}

fn normalize(samples: &mut [f32]) {
    let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    if peak > 0.0 && peak < 1.0 {
        let gain = 1.0 / peak;
        for sample in samples.iter_mut() {
            *sample *= gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(bits: u16, tag: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes()); // size, unread
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // byte rate, unread
        out.extend_from_slice(&0u16.to_le_bytes()); // block align, unread
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn silence_encodes_to_the_midpoint() {
        assert_eq!(to_unsigned_8bit(0.0), 128);
    }

    #[test]
    fn sixteen_bit_mono_round_trips() {
        let samples: Vec<u8> = [0i16, 16384, -16384]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (mono, rate) = decode(&wav(16, 1, 1, 22050, &samples)).unwrap();
        assert_eq!(rate, 22050);
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.0).abs() < 1e-6);
        assert!((mono[1] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn stereo_is_downmixed() {
        // One frame, hard left: the mono result is half of it.
        let samples: Vec<u8> = [16384i16, 0].iter().flat_map(|s| s.to_le_bytes()).collect();
        let (mono, _) = decode(&wav(16, 1, 2, 22050, &samples)).unwrap();
        assert_eq!(mono.len(), 1);
        assert!((mono[0] - 0.25).abs() < 1e-3);
    }

    #[test]
    fn resampling_halves_the_length_when_halving_the_rate() {
        let input = vec![0.0; 100];
        assert_eq!(resample_linear(&input, 44100, 22050).len(), 50);
    }

    #[test]
    fn a_file_that_is_not_a_wav_reports_rather_than_panics() {
        assert!(decode(b"this is not audio at all").is_err());
    }
}
