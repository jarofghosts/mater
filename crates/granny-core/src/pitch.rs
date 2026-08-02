//! Working out what pitch a sample already is, so playback can be tuned to the note being played.
//!
//! Nothing here comes from the hardware — the microGranny has no idea what is on its SD card and
//! simply plays it at whatever rate the DAC is clocked at. A plugin driven by microtonal MPE has to
//! do better: if a note is asked for, that note is what should sound, which means knowing the
//! sample's own pitch and playing back relative to it.
//!
//! This is the YIN algorithm (de Cheveigné & Kawahara, 2002): a squared-difference function,
//! normalised cumulatively so that low lags are not unfairly favoured, with the first dip below a
//! threshold taken as the period. Several windows are analysed and their median used, which throws
//! out the octave errors a single window occasionally produces.

/// A detected pitch.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Detection {
    pub frequency: f32,
    /// The same pitch as a fractional MIDI note number.
    pub note: f32,
    /// 0..1. Below roughly 0.5 the material is probably not pitched.
    pub confidence: f32,
}

/// Analysis window length. At 22050 Hz this is 93 ms.
const WINDOW: usize = 2048;
/// How many windows to analyse across the sample.
const MAX_WINDOWS: usize = 8;
/// Lowest and highest pitch worth looking for.
const MIN_FREQ: f32 = 40.0;
const MAX_FREQ: f32 = 2000.0;
/// YIN's absolute threshold: the first dip below this is taken as the period.
const THRESHOLD: f32 = 0.15;
/// Windows quieter than this are skipped rather than analysed as noise.
const SILENCE_RMS: f32 = 0.01;
/// A window has to be at least this convincing to vote.
const VOTE_CONFIDENCE: f32 = 0.5;
/// Below this, report nothing rather than a guess.
const MIN_CONFIDENCE: f32 = 0.35;
/// Fraction of the sample skipped up front, so an attack transient does not dominate.
const ATTACK_SKIP: f32 = 0.05;

/// A fractional MIDI note number for a frequency, A440.
pub fn note_from_frequency(frequency: f32) -> f32 {
    69.0 + 12.0 * (frequency / 440.0).log2()
}

pub fn frequency_from_note(note: f32) -> f32 {
    440.0 * ((note - 69.0) / 12.0).exp2()
}

/// Format a fractional note as a name and a cents offset, for display.
pub fn describe_note(note: f32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let nearest = note.round();
    let cents = ((note - nearest) * 100.0).round() as i32;
    let index = (nearest as i32).rem_euclid(12) as usize;
    let octave = (nearest as i32) / 12 - 1;
    if cents == 0 {
        format!("{}{}", NAMES[index], octave)
    } else {
        format!("{}{} {cents:+} ¢", NAMES[index], octave)
    }
}

/// Estimate the pitch of unsigned 8-bit mono audio played back at `rate`.
///
/// Returns `None` when the material has no clear pitch, which is the honest answer for a drum hit
/// or a noise wash.
pub fn detect_root(data: &[u8], rate: f32) -> Option<Detection> {
    if data.len() < WINDOW || rate <= 0.0 {
        return None;
    }

    let tau_min = ((rate / MAX_FREQ).ceil() as usize).max(2);
    let tau_max = ((rate / MIN_FREQ).ceil() as usize).min(WINDOW / 2);
    if tau_max <= tau_min {
        return None;
    }

    let skip = (data.len() as f32 * ATTACK_SKIP) as usize;
    let last_start = data.len().saturating_sub(WINDOW);
    if skip > last_start {
        return analyse_window(&to_float(&data[..WINDOW]), rate, tau_min, tau_max);
    }

    let span = last_start - skip;
    let mut candidates: Vec<Detection> = Vec::new();
    for index in 0..MAX_WINDOWS {
        let offset = skip + span * index / MAX_WINDOWS.max(1);
        let window = to_float(&data[offset..offset + WINDOW]);
        if rms(&window) < SILENCE_RMS {
            continue;
        }
        if let Some(detection) = analyse_window(&window, rate, tau_min, tau_max) {
            candidates.push(detection);
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // Prefer the windows that were convincing; fall back to everything if none were.
    let mut voting: Vec<&Detection> = candidates
        .iter()
        .filter(|d| d.confidence >= VOTE_CONFIDENCE)
        .collect();
    if voting.is_empty() {
        voting = candidates.iter().collect();
    }

    // The median frequency, which discards the odd octave error a single window can produce.
    voting.sort_by(|a, b| a.frequency.total_cmp(&b.frequency));
    let median = voting[voting.len() / 2];
    let confidence = voting.iter().map(|d| d.confidence).sum::<f32>() / voting.len() as f32;

    if confidence < MIN_CONFIDENCE {
        return None;
    }

    Some(Detection {
        frequency: median.frequency,
        note: median.note,
        confidence,
    })
}

fn to_float(data: &[u8]) -> Vec<f32> {
    // Unsigned 8-bit with 128 as the zero line.
    let mut window: Vec<f32> = data.iter().map(|&s| (s as f32 - 128.0) / 128.0).collect();
    let mean = window.iter().sum::<f32>() / window.len() as f32;
    for sample in &mut window {
        *sample -= mean;
    }
    window
}

fn rms(window: &[f32]) -> f32 {
    (window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32).sqrt()
}

fn analyse_window(window: &[f32], rate: f32, tau_min: usize, tau_max: usize) -> Option<Detection> {
    // YIN step 1: the squared difference function.
    let usable = window.len() - tau_max;
    let mut difference = vec![0.0f32; tau_max + 1];
    for (tau, slot) in difference
        .iter_mut()
        .enumerate()
        .take(tau_max + 1)
        .skip(tau_min)
    {
        let mut sum = 0.0;
        for i in 0..usable {
            let delta = window[i] - window[i + tau];
            sum += delta * delta;
        }
        *slot = sum;
    }

    // YIN step 2: cumulative mean normalisation, so short lags are not automatically the best.
    let mut normalised = vec![1.0f32; tau_max + 1];
    let mut running = 0.0f32;
    for tau in tau_min..=tau_max {
        running += difference[tau];
        normalised[tau] = if running > 1e-12 {
            difference[tau] * (tau - tau_min + 1) as f32 / running
        } else {
            1.0
        };
    }

    // YIN step 3: the first dip below the threshold, descending to its local minimum.
    let mut best = None;
    for tau in tau_min..tau_max {
        if normalised[tau] < THRESHOLD {
            let mut tau = tau;
            while tau + 1 < tau_max && normalised[tau + 1] < normalised[tau] {
                tau += 1;
            }
            best = Some(tau);
            break;
        }
    }
    let tau = best.unwrap_or_else(|| {
        // Nothing crossed the threshold, so take the global minimum and let the confidence
        // score say how much to trust it.
        (tau_min..=tau_max)
            .min_by(|a, b| normalised[*a].total_cmp(&normalised[*b]))
            .unwrap_or(tau_min)
    });

    let refined = parabolic_minimum(&normalised, tau, tau_min, tau_max);
    if refined <= 0.0 {
        return None;
    }

    let frequency = rate / refined;
    if !(MIN_FREQ..=MAX_FREQ).contains(&frequency) {
        return None;
    }

    Some(Detection {
        frequency,
        note: note_from_frequency(frequency),
        confidence: (1.0 - normalised[tau]).clamp(0.0, 1.0),
    })
}

/// Fit a parabola through the minimum and its neighbours, for sub-sample period resolution.
fn parabolic_minimum(values: &[f32], tau: usize, tau_min: usize, tau_max: usize) -> f32 {
    if tau <= tau_min || tau >= tau_max {
        return tau as f32;
    }
    let previous = values[tau - 1];
    let current = values[tau];
    let next = values[tau + 1];
    let denominator = 2.0 * (2.0 * current - next - previous);
    if denominator.abs() < 1e-12 {
        return tau as f32;
    }
    tau as f32 + (next - previous) / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 22050.0;

    fn render(frequency: f32, seconds: f32, harmonics: usize) -> Vec<u8> {
        let count = (RATE * seconds) as usize;
        (0..count)
            .map(|i| {
                let t = i as f32 / RATE;
                let mut value = 0.0;
                for harmonic in 1..=harmonics {
                    value += (2.0 * std::f32::consts::PI * frequency * harmonic as f32 * t).sin()
                        / harmonic as f32;
                }
                let scaled = (value * 0.5).clamp(-1.0, 1.0);
                ((scaled * 127.0) + 128.0).round().clamp(0.0, 255.0) as u8
            })
            .collect()
    }

    fn noise(seconds: f32) -> Vec<u8> {
        let mut rng = crate::rng::Xorshift96::new();
        (0..(RATE * seconds) as usize)
            .map(|_| rng.rand(256) as u8)
            .collect()
    }

    #[test]
    fn detects_a_sine() {
        // 220 Hz is A3, MIDI note 57.
        let detection = detect_root(&render(220.0, 1.0, 1), RATE).expect("should find a pitch");
        assert!(
            (detection.note - 57.0).abs() < 0.1,
            "got {} ({} Hz)",
            detection.note,
            detection.frequency
        );
        assert!(detection.confidence > 0.8, "got {}", detection.confidence);
    }

    #[test]
    fn detects_a_harmonically_rich_tone_without_octave_error() {
        // A sawtooth-ish stack is where naive autocorrelation tends to answer an octave low.
        let detection = detect_root(&render(110.0, 1.0, 12), RATE).expect("should find a pitch");
        assert!(
            (detection.note - 45.0).abs() < 0.15,
            "expected A2 (45), got {}",
            detection.note
        );
    }

    #[test]
    fn tracks_pitch_across_the_useful_range() {
        for (frequency, expected) in [(65.4, 36.0), (261.6, 60.0), (880.0, 81.0)] {
            let detection =
                detect_root(&render(frequency, 0.6, 4), RATE).expect("should find a pitch");
            assert!(
                (detection.note - expected).abs() < 0.2,
                "{frequency} Hz: expected note {expected}, got {}",
                detection.note
            );
        }
    }

    #[test]
    fn reports_nothing_for_noise() {
        match detect_root(&noise(1.0), RATE) {
            None => {}
            Some(detection) => assert!(
                detection.confidence < 0.6,
                "noise should not look confidently pitched, got {}",
                detection.confidence
            ),
        }
    }

    #[test]
    fn reports_nothing_for_silence_or_scraps() {
        assert!(detect_root(&vec![128u8; 40_000], RATE).is_none());
        assert!(detect_root(&[128, 129, 130], RATE).is_none());
        assert!(detect_root(&[], RATE).is_none());
    }

    #[test]
    fn note_and_frequency_round_trip() {
        for note in [21.0f32, 45.5, 59.0, 69.0, 108.0] {
            let back = note_from_frequency(frequency_from_note(note));
            assert!((back - note).abs() < 1e-3, "{note} -> {back}");
        }
        assert!((frequency_from_note(69.0) - 440.0).abs() < 1e-3);
    }

    #[test]
    fn note_names_read_the_way_a_musician_expects() {
        assert_eq!(describe_note(60.0), "C4");
        assert_eq!(describe_note(69.0), "A4");
        assert_eq!(describe_note(59.0), "B3");
        assert_eq!(describe_note(60.25), "C4 +25 ¢");
        assert_eq!(describe_note(59.9), "C4 -10 ¢");
    }
}
