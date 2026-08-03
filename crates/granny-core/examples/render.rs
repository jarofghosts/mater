//! Render a note offline, so the engine can be listened to without a host.
//!
//! ```text
//! cargo run -p granny-core --example render -- in.wav out.wav note=59 grain=30 shift=200
//! cargo run -p granny-core --example render -- in.wav out.wav chord=48,55,59 seconds=6
//! ```
//!
//! Deliberately uses a hand-rolled WAV reader and writer so `granny-core` keeps zero dependencies.

use granny_core::params::{Fidelity, HwParams, ModSlot, MOD_SLOTS};
use granny_core::{Engine, Expression, Scene, TransportInfo, Tuning, VoiceKey};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::ExitCode;

const SAMPLE_RATE: f32 = 48_000.0;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: render <input.wav> <output.wav> [key=value ...]\n\n\
             knobs:    rate crush attack release grain shift start end\n\
             settings: tuned legato repeat sync random hold level\n\
             fidelity: curve=hardware|extended interp=0|1 quantize=0|1 fade=<ms>\n\
             tuning:   match=0|1 root=<midi note> table=et|hardware snap=<edo>\n\
             playback: note=<n> chord=<n,n,n> velocity=<0..1> seconds=<s> bend=<semitones>"
        );
        return ExitCode::FAILURE;
    }

    let opts = parse_options(&args[2..]);
    let unknown: Vec<&String> = opts.keys().filter(|k| !is_known(k)).collect();
    if !unknown.is_empty() {
        eprintln!("unknown options: {unknown:?}");
        return ExitCode::FAILURE;
    }

    let raw = match fs::read(&args[0]) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("cannot read {}: {err}", args[0]);
            return ExitCode::FAILURE;
        }
    };
    let (mono, source_rate) = match decode_wav(&raw) {
        Ok(decoded) => decoded,
        Err(err) => {
            eprintln!("cannot decode {}: {err}", args[0]);
            return ExitCode::FAILURE;
        }
    };

    let sample = granny_core::SampleBuffer::new(&args[0], mono, source_rate);
    println!(
        "loaded {} samples at {} Hz ({:.2} s of audio at the native 22050 Hz)",
        sample.len(),
        source_rate,
        sample.len() as f32 / 22050.0
    );

    let params = HwParams {
        rate: opts.int("rate", 877),
        crush: opts.int("crush", 0),
        attack: opts.int("attack", 0),
        release: opts.int("release", 0),
        grain: opts.int("grain", 0),
        shift: opts.int("shift", 128),
        start: opts.int("start", 0),
        end: opts.int("end", 1022),
        tuned: opts.flag("tuned", true),
        legato: opts.flag("legato", false),
        repeat: opts.flag("repeat", true),
        sync: opts.flag("sync", false),
        random_shift: opts.flag("random", false),
        hold: opts.flag("hold", false),
        level: opts.float("level", 0.5),
    };

    let fidelity = Fidelity {
        curve_mode: match opts.string("curve", "hardware").as_str() {
            "extended" => granny_core::curves::CurveMode::Extended,
            _ => granny_core::curves::CurveMode::HardwareExact,
        },
        interpolate: opts.flag("interp", false),
        quantize_seeks: opts.flag("quantize", true),
        grain_fade_ms: opts.float("fade", 0.0),
    };

    // What pitch the sample already is, so a played note can be transposed to sound like itself.
    let detected = sample.detected_root();
    match detected {
        Some(d) => println!(
            "detected root: {} ({:.1} Hz, {:.0} % confident)",
            granny_core::pitch::describe_note(d.note),
            d.frequency,
            d.confidence * 100.0
        ),
        None => println!("detected root: none — rooting on B3, as the hardware does"),
    }

    let matching = opts.flag("match", true);
    let default_root = detected
        .filter(|_| matching)
        .map_or(granny_core::tables::NATIVE_NOTE, |d| d.note);

    let tuning = Tuning {
        table: match opts.string("table", "et").as_str() {
            "hardware" | "hw" => granny_core::PitchTable::Hardware,
            _ => granny_core::PitchTable::EqualTemperament,
        },
        snap_divisions: match opts.int("snap", 0) {
            0 => None,
            divisions => Some(divisions as u32),
        },
        root_note: opts.float("root", default_root),
        scala: None,
    };

    let notes: Vec<u8> = match opts.raw.get("chord") {
        Some(list) => list
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect(),
        None => vec![opts.int("note", 59) as u8],
    };
    let velocity = opts.float("velocity", 1.0);
    let seconds = opts.float("seconds", 4.0);
    // Bend is given in semitones here. A host resolves a wheel position against a bend range
    // before the engine ever sees it, so there is no range to configure offline.
    let expression = Expression {
        bend_semitones: opts.float("bend", 0.0),
        ..Default::default()
    };

    let mods = [ModSlot::default(); MOD_SLOTS];
    let scene = Scene {
        sample: &sample,
        params: &params,
        fidelity: &fidelity,
        tuning: &tuning,
        mods: &mods,
        transport: TransportInfo::default(),
    };

    let mut engine = Engine::new(16, SAMPLE_RATE);
    for note in &notes {
        engine.note_on(
            &scene,
            VoiceKey {
                voice_id: granny_core::fallback_voice_id(0, *note),
                channel: 0,
                note: *note,
            },
            velocity,
            expression.clone(),
        );
    }

    let total = (seconds * SAMPLE_RATE) as usize;
    let mut left = vec![0.0f32; total];
    let mut right = vec![0.0f32; total];
    // Render in modest blocks, the way a host would.
    let mut offset = 0;
    while offset < total {
        let run = 512.min(total - offset);
        engine.process(
            &scene,
            &mut left[offset..offset + run],
            &mut right[offset..offset + run],
        );
        offset += run;
    }

    let peak = left
        .iter()
        .chain(right.iter())
        .fold(0.0f32, |acc, s| acc.max(s.abs()));
    println!(
        "rendered {seconds:.1} s, {} notes, peak {:.3} ({:.1} dBFS)",
        notes.len(),
        peak,
        20.0 * peak.max(1e-9).log10()
    );

    // Run the render back through the detector, so the tool can say whether the note that was
    // asked for is the note that came out.
    if notes.len() == 1 && peak > 0.001 {
        let analysed: Vec<u8> = left
            .iter()
            .map(|s| ((s / peak * 120.0) + 128.0).round().clamp(0.0, 255.0) as u8)
            .collect();
        match granny_core::pitch::detect_root(&analysed, SAMPLE_RATE) {
            Some(detection) => {
                let wanted = granny_core::pitch::frequency_from_note(notes[0] as f32);
                let cents = 1200.0 * (detection.frequency / wanted).log2();
                println!(
                    "output pitch: {} ({:.1} Hz) — note {} wants {:.1} Hz, {cents:+.1} cents off",
                    granny_core::pitch::describe_note(detection.note),
                    detection.frequency,
                    notes[0],
                    wanted
                );
            }
            None => println!("output pitch: no clear pitch in the render"),
        }
    }

    match fs::write(&args[1], encode_wav(&left, &right, SAMPLE_RATE as u32)) {
        Ok(()) => {
            println!("wrote {}", args[1]);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("cannot write {}: {err}", args[1]);
            ExitCode::FAILURE
        }
    }
}

const KNOWN: &[&str] = &[
    "rate", "crush", "attack", "release", "grain", "shift", "start", "end", "tuned", "legato",
    "repeat", "sync", "random", "hold", "level", "bend", "curve", "interp", "quantize", "fade",
    "note", "chord", "velocity", "seconds", "snap", "match", "root", "table",
];

fn is_known(key: &str) -> bool {
    KNOWN.contains(&key)
}

struct Options {
    raw: HashMap<String, String>,
}

impl Options {
    fn keys(&self) -> impl Iterator<Item = &String> {
        self.raw.keys()
    }
    fn int(&self, key: &str, default: u16) -> u16 {
        self.raw
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    fn float(&self, key: &str, default: f32) -> f32 {
        self.raw
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    fn flag(&self, key: &str, default: bool) -> bool {
        match self.raw.get(key).map(String::as_str) {
            Some("1" | "true" | "on" | "yes") => true,
            Some("0" | "false" | "off" | "no") => false,
            _ => default,
        }
    }
    fn string(&self, key: &str, default: &str) -> String {
        self.raw.get(key).cloned().unwrap_or_else(|| default.into())
    }
}

fn parse_options(args: &[String]) -> Options {
    let raw = args
        .iter()
        .filter_map(|arg| {
            arg.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    Options { raw }
}

/// Decode a PCM WAV into unsigned 8-bit mono, which is what the hardware would have on its card.
fn decode_wav(bytes: &[u8]) -> Result<(Vec<u8>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }

    let mut pos = 12;
    let mut channels = 1u16;
    let mut rate = 22050u32;
    let mut bits = 16u16;
    let mut format = 1u16;
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());

        match id {
            b"fmt " if size >= 16 => {
                let f = &bytes[body_start..body_end];
                format = u16::from_le_bytes(f[0..2].try_into().unwrap());
                channels = u16::from_le_bytes(f[2..4].try_into().unwrap()).max(1);
                rate = u32::from_le_bytes(f[4..8].try_into().unwrap());
                bits = u16::from_le_bytes(f[14..16].try_into().unwrap());
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }
        // Chunks are word aligned.
        pos = body_start + size + (size & 1);
    }

    if format != 1 {
        return Err(format!("unsupported WAV format tag {format}, expected PCM"));
    }
    let data = data.ok_or("no data chunk")?;
    let channels = channels as usize;

    let mono: Vec<u8> = match bits {
        8 => data
            .chunks_exact(channels)
            .map(|frame| {
                let sum: u32 = frame.iter().map(|&s| s as u32).sum();
                (sum / channels as u32) as u8
            })
            .collect(),
        16 => data
            .chunks_exact(2 * channels)
            .map(|frame| {
                let sum: i32 = frame
                    .chunks_exact(2)
                    .map(|s| i16::from_le_bytes([s[0], s[1]]) as i32)
                    .sum();
                let averaged = sum / channels as i32;
                // Signed 16-bit down to unsigned 8-bit, exactly as the SD card would hold it.
                ((averaged >> 8) + 128).clamp(0, 255) as u8
            })
            .collect(),
        other => return Err(format!("unsupported bit depth {other}, expected 8 or 16")),
    };

    Ok((mono, rate))
}

fn encode_wav(left: &[f32], right: &[f32], rate: u32) -> Vec<u8> {
    let frames = left.len().min(right.len());
    let data_len = frames * 4;
    let mut out = Vec::with_capacity(44 + data_len);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes()); // byte rate
    out.extend_from_slice(&4u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());

    for i in 0..frames {
        for channel in [left, right] {
            let clamped = (channel[i].clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            out.extend_from_slice(&clamped.to_le_bytes());
        }
    }

    out
}
