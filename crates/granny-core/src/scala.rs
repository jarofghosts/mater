//! Scala `.scl` scale and `.kbm` keyboard-map parsing.
//!
//! Nothing here exists on the hardware. It is the general case of the quarter-tone support the
//! plugin needs: a scale file turns a MIDI note into a fractional note number, which then drives
//! the same rate lookup every other note uses.

use std::fmt;

#[derive(Debug)]
pub enum ScalaError {
    Empty,
    BadNoteCount(String),
    BadPitch(String),
    Truncated { expected: usize, found: usize },
    BadKeymapField(&'static str, String),
}

impl fmt::Display for ScalaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalaError::Empty => write!(f, "file contains no data lines"),
            ScalaError::BadNoteCount(s) => write!(f, "invalid note count {s:?}"),
            ScalaError::BadPitch(s) => write!(f, "invalid pitch {s:?}"),
            ScalaError::Truncated { expected, found } => {
                write!(f, "expected {expected} pitches, found {found}")
            }
            ScalaError::BadKeymapField(name, s) => write!(f, "invalid {name} {s:?}"),
        }
    }
}

impl std::error::Error for ScalaError {}

/// A parsed `.scl` scale. `cents` holds degrees 1..=n; the last entry is the period (usually 1200).
#[derive(Clone, Debug, PartialEq)]
pub struct ScalaScale {
    pub description: String,
    pub cents: Vec<f64>,
}

impl Default for ScalaScale {
    /// 12-tone equal temperament, so a `Tuning` with a default scale behaves like no scale at all.
    fn default() -> Self {
        Self {
            description: "12-EDO".into(),
            cents: (1..=12).map(|d| d as f64 * 100.0).collect(),
        }
    }
}

impl ScalaScale {
    pub fn degrees(&self) -> usize {
        self.cents.len()
    }

    pub fn period_cents(&self) -> f64 {
        self.cents.last().copied().unwrap_or(1200.0)
    }

    /// Cents above the root for a scale degree. Degree 0 is the implicit 1/1.
    pub fn degree_cents(&self, degree: usize) -> f64 {
        if degree == 0 {
            0.0
        } else {
            self.cents[(degree - 1).min(self.cents.len() - 1)]
        }
    }

    pub fn parse(text: &str) -> Result<Self, ScalaError> {
        let mut lines = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('!'))
            .map(str::trim);

        let description = lines.next().ok_or(ScalaError::Empty)?.to_string();
        let count_line = lines.next().ok_or(ScalaError::Empty)?;
        let count: usize = count_line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|_| ScalaError::BadNoteCount(count_line.to_string()))?;

        let mut cents = Vec::with_capacity(count);
        for line in lines {
            if cents.len() == count {
                break;
            }
            // Scala allows trailing commentary after the value on a pitch line.
            let token = match line.split_whitespace().next() {
                Some(t) => t,
                None => continue,
            };
            cents.push(parse_pitch(token)?);
        }

        if cents.len() != count {
            return Err(ScalaError::Truncated {
                expected: count,
                found: cents.len(),
            });
        }

        Ok(Self { description, cents })
    }
}

/// A pitch line is cents if it contains a decimal point, otherwise a ratio.
fn parse_pitch(token: &str) -> Result<f64, ScalaError> {
    let bad = || ScalaError::BadPitch(token.to_string());
    if token.contains('.') {
        token.parse::<f64>().map_err(|_| bad())
    } else if let Some((num, den)) = token.split_once('/') {
        let num: f64 = num.parse().map_err(|_| bad())?;
        let den: f64 = den.parse().map_err(|_| bad())?;
        if num <= 0.0 || den <= 0.0 {
            return Err(bad());
        }
        Ok(1200.0 * (num / den).log2())
    } else {
        let num: f64 = token.parse().map_err(|_| bad())?;
        if num <= 0.0 {
            return Err(bad());
        }
        Ok(1200.0 * num.log2())
    }
}

/// A parsed `.kbm` keyboard mapping.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyboardMap {
    pub size: usize,
    pub first_note: i32,
    pub last_note: i32,
    /// The note that sounds the scale's 1/1.
    pub middle_note: i32,
    pub reference_note: i32,
    pub reference_freq: f64,
    /// Scale degree treated as the formal octave; 0 means "the size of the map".
    pub formal_octave: usize,
    /// One entry per map position: the scale degree it plays, or `None` if unmapped.
    pub mapping: Vec<Option<usize>>,
}

impl KeyboardMap {
    pub fn parse(text: &str) -> Result<Self, ScalaError> {
        let mut fields = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('!') && !l.is_empty());

        fn take<'a>(
            it: &mut impl Iterator<Item = &'a str>,
            name: &'static str,
        ) -> Result<&'a str, ScalaError> {
            it.next().ok_or(ScalaError::BadKeymapField(name, "".into()))
        }
        fn num<T: std::str::FromStr>(s: &str, name: &'static str) -> Result<T, ScalaError> {
            s.split_whitespace()
                .next()
                .unwrap_or("")
                .parse()
                .map_err(|_| ScalaError::BadKeymapField(name, s.to_string()))
        }

        let size: usize = num(take(&mut fields, "size")?, "size")?;
        let first_note = num(take(&mut fields, "first note")?, "first note")?;
        let last_note = num(take(&mut fields, "last note")?, "last note")?;
        let middle_note = num(take(&mut fields, "middle note")?, "middle note")?;
        let reference_note = num(take(&mut fields, "reference note")?, "reference note")?;
        let reference_freq = num(
            take(&mut fields, "reference frequency")?,
            "reference frequency",
        )?;
        let formal_octave = num(take(&mut fields, "formal octave")?, "formal octave")?;

        let mut mapping = Vec::with_capacity(size);
        for line in fields {
            if mapping.len() == size {
                break;
            }
            let token = line.split_whitespace().next().unwrap_or("");
            mapping.push(match token {
                "x" | "X" => None,
                other => Some(
                    other
                        .parse()
                        .map_err(|_| ScalaError::BadKeymapField("mapping entry", line.into()))?,
                ),
            });
        }
        // A short map is legal; the remaining positions are unmapped.
        mapping.resize(size, None);

        Ok(Self {
            size,
            first_note,
            last_note,
            middle_note,
            reference_note,
            reference_freq,
            formal_octave,
            mapping,
        })
    }
}

/// A scale plus its keyboard mapping, able to answer "what fractional MIDI note is this key?".
#[derive(Clone, Debug)]
pub struct ScalaTuning {
    pub scale: ScalaScale,
    pub keymap: Option<KeyboardMap>,
}

/// Default keyboard mapping when no `.kbm` is supplied: 1/1 on middle C, A440 as reference.
const DEFAULT_MIDDLE_NOTE: i32 = 60;
const DEFAULT_REFERENCE_NOTE: i32 = 69;
const DEFAULT_REFERENCE_FREQ: f64 = 440.0;

impl ScalaTuning {
    pub fn new(scale: ScalaScale, keymap: Option<KeyboardMap>) -> Self {
        Self { scale, keymap }
    }

    fn middle_note(&self) -> i32 {
        self.keymap
            .as_ref()
            .map_or(DEFAULT_MIDDLE_NOTE, |k| k.middle_note)
    }

    fn reference(&self) -> (i32, f64) {
        self.keymap
            .as_ref()
            .map_or((DEFAULT_REFERENCE_NOTE, DEFAULT_REFERENCE_FREQ), |k| {
                (k.reference_note, k.reference_freq)
            })
    }

    /// Cents above the scale's 1/1 for a MIDI note, or `None` if the key is unmapped.
    pub fn cents_for_note(&self, note: i32) -> Option<f64> {
        let index = note - self.middle_note();
        match &self.keymap {
            None => {
                let degrees = self.scale.degrees() as i32;
                let octave = index.div_euclid(degrees);
                let degree = index.rem_euclid(degrees) as usize;
                Some(octave as f64 * self.scale.period_cents() + self.scale.degree_cents(degree))
            }
            Some(k) => {
                if k.size == 0 {
                    return None;
                }
                if note < k.first_note || note > k.last_note {
                    return None;
                }
                let size = k.size as i32;
                let octave = index.div_euclid(size);
                let position = index.rem_euclid(size) as usize;
                let degree = k.mapping[position]?;
                let octave_degree = if k.formal_octave == 0 {
                    k.size
                } else {
                    k.formal_octave
                };
                let period = self.scale.degree_cents(octave_degree);
                Some(octave as f64 * period + self.scale.degree_cents(degree))
            }
        }
    }

    /// The fractional MIDI note (in 12-ET terms) that sounds this key.
    ///
    /// Everything downstream — the hardware rate table, equal temperament, MPE bend — works in
    /// fractional note numbers, so a scale only has to say where a key lands on that axis.
    pub fn effective_note(&self, note: i32) -> Option<f32> {
        let cents = self.cents_for_note(note)?;
        let (ref_note, ref_freq) = self.reference();
        // Where the reference key sits in the scale, so the reference frequency lands on it.
        let ref_cents = self.cents_for_note(ref_note).unwrap_or(0.0);
        let ref_note_12tet = 69.0 + 12.0 * (ref_freq / 440.0).log2();
        Some((ref_note_12tet + (cents - ref_cents) / 100.0) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDO24: &str = "\
! 24edo.scl
!
24 equal divisions of the octave
 24
!
 50.000
 100.000
 150.000
 200.000
 250.000
 300.000
 350.000
 400.000
 450.000
 500.000
 550.000
 600.000
 650.000
 700.000
 750.000
 800.000
 850.000
 900.000
 950.000
 1000.000
 1050.000
 1100.000
 1150.000
 2/1
";

    #[test]
    fn parses_a_24_edo_scale() {
        let scale = ScalaScale::parse(EDO24).unwrap();
        assert_eq!(scale.description, "24 equal divisions of the octave");
        assert_eq!(scale.degrees(), 24);
        assert!((scale.period_cents() - 1200.0).abs() < 1e-9);
        for degree in 0..=24 {
            let expected = degree as f64 * 50.0;
            assert!(
                (scale.degree_cents(degree) - expected).abs() < 1e-9,
                "degree {degree}"
            );
        }
    }

    #[test]
    fn quarter_tones_land_on_half_semitones() {
        let tuning = ScalaTuning::new(ScalaScale::parse(EDO24).unwrap(), None);
        // With 24 degrees per octave, one key step is half a semitone.
        let base = tuning.effective_note(60).unwrap();
        assert!((tuning.effective_note(61).unwrap() - base - 0.5).abs() < 1e-4);
        assert!((tuning.effective_note(84).unwrap() - base - 12.0).abs() < 1e-4);
    }

    #[test]
    fn twelve_edo_scale_is_an_identity_mapping() {
        let tuning = ScalaTuning::new(ScalaScale::default(), None);
        for note in [0, 21, 60, 69, 127] {
            let effective = tuning.effective_note(note).unwrap();
            assert!(
                (effective - note as f32).abs() < 1e-3,
                "note {note} -> {effective}"
            );
        }
    }

    #[test]
    fn parses_ratios_and_bare_integers() {
        let scl = "!\ntest\n 3\n 3/2\n 2\n 1200.0\n";
        let scale = ScalaScale::parse(scl).unwrap();
        assert!((scale.cents[0] - 701.955).abs() < 0.01);
        assert!((scale.cents[1] - 1200.0).abs() < 0.01);
    }

    #[test]
    fn rejects_a_truncated_scale() {
        let scl = "!\ntest\n 5\n 100.0\n 200.0\n";
        assert!(matches!(
            ScalaScale::parse(scl),
            Err(ScalaError::Truncated { .. })
        ));
    }

    #[test]
    fn keymap_leaves_unmapped_keys_silent() {
        // A 12-position map that only sounds the white keys of C major.
        let kbm = "\
! test.kbm
12
0
127
60
69
440.0
0
0
x
1
x
2
3
x
4
x
5
x
6
";
        let map = KeyboardMap::parse(kbm).unwrap();
        assert_eq!(map.size, 12);
        assert_eq!(map.mapping[0], Some(0));
        assert_eq!(map.mapping[1], None);

        let tuning = ScalaTuning::new(ScalaScale::default(), Some(map));
        assert!(tuning.effective_note(61).is_none(), "C# is unmapped");
        assert!(tuning.effective_note(60).is_some());
    }

    #[test]
    fn keymap_reference_frequency_shifts_the_whole_scale() {
        let kbm = "1\n0\n127\n60\n69\n880.0\n0\n0\n";
        let map = KeyboardMap::parse(kbm).unwrap();
        let tuning = ScalaTuning::new(ScalaScale::default(), Some(map));
        // A 1-position map is one scale degree per key; the reference is an octave sharp.
        let a = tuning.effective_note(69).unwrap();
        assert!((a - 81.0).abs() < 1e-3, "got {a}");
    }
}
