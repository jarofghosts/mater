//! Decoding audio files into the 8-bit mono buffer the engine expects, and keeping it in the
//! plugin's state so an instance is a self-contained preset.
//!
//! The hardware plays raw bytes off an SD card and does not care what rate the file was recorded
//! at — the DAC clock alone sets the pitch. That is faithful but surprising, so by default a loaded
//! file is resampled to 22050 Hz, which makes MIDI note 59 play it back at its original speed. Turn
//! that off for the literal hardware behaviour.

use base64::engine::general_purpose::STANDARD as BASE64;
use granny_core::sample::SampleBuffer;
use granny_core::scala::{KeyboardMap, ScalaScale, ScalaTuning};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::Path;
use symphonia::core::audio::SampleBuffer as SymphoniaBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// The rate the stock sample bank is recorded at, and what note 59 plays back at.
pub const NATIVE_RATE: u32 = 22050;

/// A decoded sample, stored inside the plugin's state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredSample {
    pub name: String,
    /// Where it came from, for the editor's "reload" affordance. Not required to restore state.
    pub path: String,
    pub source_rate: u32,
    /// Unsigned 8-bit mono, base64 so the state stays compact.
    #[serde(with = "base64_bytes", default)]
    pub data: Vec<u8>,
}

impl StoredSample {
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn to_buffer(&self) -> SampleBuffer {
        SampleBuffer::new(self.name.clone(), self.data.clone(), self.source_rate)
    }
}

/// Scala files, stored as their original text so a preset carries its own tuning.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredScale {
    pub scl_name: String,
    pub scl: String,
    pub kbm_name: String,
    pub kbm: String,
}

impl StoredScale {
    pub fn is_empty(&self) -> bool {
        self.scl.trim().is_empty()
    }

    /// Parse into a usable tuning, or explain why not.
    pub fn to_tuning(&self) -> Result<Option<ScalaTuning>, String> {
        if self.is_empty() {
            return Ok(None);
        }
        let scale = ScalaScale::parse(&self.scl).map_err(|e| format!("{}: {e}", self.scl_name))?;
        let keymap = if self.kbm.trim().is_empty() {
            None
        } else {
            Some(KeyboardMap::parse(&self.kbm).map_err(|e| format!("{}: {e}", self.kbm_name))?)
        };
        Ok(Some(ScalaTuning::new(scale, keymap)))
    }
}

/// How a file should be conditioned on the way in.
#[derive(Copy, Clone, Debug)]
pub struct LoadOptions {
    /// Resample to 22050 Hz so note 59 plays the file at its original speed.
    pub resample: bool,
    /// Scale to full range. The hardware's own manual recommends loud samples.
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

/// Decode any supported audio file into a [`StoredSample`].
pub fn load_file(path: &Path, options: LoadOptions) -> Result<StoredSample, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open file: {e}"))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("unrecognised audio format: {e}"))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.sample_rate.is_some())
        .or_else(|| format.tracks().first())
        .ok_or("file contains no audio tracks")?;
    let track_id = track.id;
    let source_rate = track.codec_params.sample_rate.unwrap_or(NATIVE_RATE);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("no decoder for this file: {e}"))?;

    let mut mono: Vec<f32> = Vec::new();
    let mut interleaved: Option<SymphoniaBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // Symphonia signals the end of a stream with an EOF io error.
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("read error: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let channels = spec.channels.count().max(1);
                let buffer = interleaved.get_or_insert_with(|| {
                    SymphoniaBuffer::<f32>::new(decoded.capacity() as u64, spec)
                });
                buffer.copy_interleaved_ref(decoded);
                for frame in buffer.samples().chunks(channels) {
                    mono.push(frame.iter().sum::<f32>() / channels as f32);
                }
            }
            // A damaged packet should not sink the whole load.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode error: {e}")),
        }
    }

    if mono.is_empty() {
        return Err("file decoded to no audio".into());
    }

    if options.resample && source_rate != NATIVE_RATE {
        mono = resample_linear(&mono, source_rate, NATIVE_RATE);
    }
    if options.normalize {
        normalize(&mut mono);
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sample")
        .to_string();

    Ok(StoredSample {
        name,
        path: path.to_string_lossy().into_owned(),
        source_rate: if options.resample {
            NATIVE_RATE
        } else {
            source_rate
        },
        data: mono.iter().map(|&s| to_unsigned_8bit(s)).collect(),
    })
}

/// Float sample to the unsigned 8-bit encoding the DAC ISR reads, where 128 is silence.
fn to_unsigned_8bit(sample: f32) -> u8 {
    ((sample.clamp(-1.0, 1.0) * 127.0) + 128.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Linear resampling. The engine is an 8-bit sampler with no interpolation of its own, so anything
/// fancier here would be polish nobody can hear.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((input.len() as f64) / ratio).round().max(1.0) as usize;
    (0..out_len)
        .map(|i| {
            let position = i as f64 * ratio;
            let index = position.floor() as usize;
            let frac = (position - index as f64) as f32;
            let a = input[index.min(input.len() - 1)];
            let b = input[(index + 1).min(input.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

fn normalize(samples: &mut [f32]) {
    let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    if peak > 0.0001 && peak < 0.999 {
        let gain = 0.999 / peak;
        for sample in samples.iter_mut() {
            *sample *= gain;
        }
    }
}

/// Read a text file, for `.scl` and `.kbm`.
pub fn load_text(path: &Path) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read file: {e}"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("scale")
        .to_string();
    Ok((name, text))
}

/// Serialize the sample payload as base64 rather than a JSON array of thousands of numbers.
mod base64_bytes {
    use super::{Deserialize, Deserializer, Serializer, BASE64};
    use base64::Engine as _;

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        BASE64
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_encodes_to_the_midpoint() {
        assert_eq!(to_unsigned_8bit(0.0), 128);
        assert_eq!(to_unsigned_8bit(1.0), 255);
        assert_eq!(to_unsigned_8bit(-1.0), 1);
        assert_eq!(to_unsigned_8bit(-5.0), 1, "clamps");
    }

    #[test]
    fn resampling_halves_the_length_when_halving_the_rate() {
        let input: Vec<f32> = (0..1000).map(|i| (i as f32 / 100.0).sin()).collect();
        let output = resample_linear(&input, 44100, 22050);
        assert_eq!(output.len(), 500);
    }

    #[test]
    fn resampling_is_a_no_op_at_the_same_rate() {
        let input = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_linear(&input, 22050, 22050), input);
    }

    #[test]
    fn normalize_scales_quiet_material_up() {
        let mut samples = vec![0.1, -0.05, 0.02];
        normalize(&mut samples);
        assert!((samples[0] - 0.999).abs() < 1e-3);
    }

    #[test]
    fn stored_sample_round_trips_through_json() {
        let stored = StoredSample {
            name: "kick.wav".into(),
            path: "/tmp/kick.wav".into(),
            source_rate: 22050,
            data: (0..300).map(|i| (i % 256) as u8).collect(),
        };
        let json = serde_json::to_string(&stored).unwrap();
        assert!(
            !json.contains("[1,2,3"),
            "payload should be base64, not an array"
        );
        let restored: StoredSample = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.data, stored.data);
        assert_eq!(restored.name, stored.name);
    }

    #[test]
    fn empty_scale_yields_no_tuning() {
        assert!(StoredScale::default().to_tuning().unwrap().is_none());
    }

    #[test]
    fn a_broken_scale_reports_rather_than_panics() {
        let scale = StoredScale {
            scl_name: "bad.scl".into(),
            scl: "!\ndescription\nnot-a-number\n".into(),
            ..Default::default()
        };
        assert!(scale.to_tuning().is_err());
    }
}
