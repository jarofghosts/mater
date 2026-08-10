//! The state blob: one JSON object holding everything needed to reproduce an instance.
//!
//! Schwung rides three separate features on this single opaque string — per-slot autosave
//! (`slot_N.json`), chain patches, and User Presets — all through `get_param("state")` and
//! `set_param("state", blob)`. `docs/MODULES.md` in the Schwung tree calls it the state contract.
//!
//! # The two halves are not symmetric
//!
//! **Writing runs on the audio thread**, every autosave tick, so [`write`] allocates nothing: it
//! formats straight into the host's own 64 KB buffer through a [`SliceWriter`], and base64 is
//! encoded four characters at a time into that same buffer rather than into a `String`.
//!
//! **Reading happens on slot load and preset recall**, which is already the path that decodes a
//! sample and is already expected to glitch. [`restore`] uses `serde_json`, allocates freely, and
//! is lenient about a blob that something else has reformatted along the way.
//!
//! # What is in it
//!
//! Every knob and switch is an ordinary parameter, so the blob is really just *the parameters*
//! plus the two things a parameter cannot carry: the sample's bytes and the Scala text.
//!
//! ```json
//! {"v":1,
//!  "params":{"rate":"877","crush":"0", ...},
//!  "scala":{"scl":"! 12-tone...","kbm":""},
//!  "sample":{"name":"kick","path":"/data/.../kick.wav","rate":22050,"data":"<base64>"}}
//! ```
//!
//! Values are strings because the parameter interface is stringly-typed anyway; going through
//! `set_param` on the way back in means a restored blob takes exactly the same path as a knob
//! turn, clamping and all.
//!
//! # Self-contained, up to a point
//!
//! `SHADOW_PARAM_VALUE_LEN` is 64 KB, and base64 costs a third on top, so about 45 KB of audio
//! fits — two seconds at 22050 Hz. Under that, `data` is embedded and the preset is genuinely
//! self-contained, which is the contract `docs/MODULES.md` asks for and is most of what an 8-bit
//! grain sampler ever holds. Over it, only `path` is written and recall re-reads the file. That
//! degradation is announced in the blob itself: `"embedded":false` says the audio is by reference,
//! so a preset moved to another device can say why it came up silent instead of just doing it.

use core::fmt::{self, Write};
use std::ffi::c_char;
use std::os::raw::c_int;

use granny_core::SampleBuffer;

use crate::bridge::{self, Scratch};
use crate::Instance;

/// Bumped only if the shape around the parameters changes. Individual parameters coming and going
/// needs no version: an unknown key is skipped and a missing one keeps its default.
const VERSION: u32 = 1;

/// Slack kept free so the root object can always close after the sample has been written.
const TAIL_RESERVE: usize = 8;

// --- writing (audio thread, no allocation) -------------------------------------------------------

/// A `fmt::Write` over a fixed buffer that refuses to overflow rather than growing.
pub struct SliceWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> SliceWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// How much room is left.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Rewind to a mark taken earlier, discarding everything written since.
    ///
    /// This is what lets the sample *try* to embed its audio and fall back to a path if it does
    /// not fit, rather than predicting the answer from an estimate that could be wrong in the
    /// direction that loses the whole blob.
    pub fn truncate(&mut self, mark: usize) {
        self.len = mark.min(self.len);
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> fmt::Result {
        if bytes.len() > self.remaining() {
            return Err(fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

impl Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes())
    }
}

/// Serialise the instance into the host's buffer. Returns the length written, or -1.
///
/// # Safety
///
/// `buf` must be writable for `buf_len` bytes, which is what `get_param` guarantees.
pub fn write(inst: &Instance, buf: *mut c_char, buf_len: c_int) -> c_int {
    if buf.is_null() || buf_len <= 1 {
        return -1;
    }

    // One byte held back for the NUL the host expects.
    let capacity = buf_len as usize - 1;
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, capacity) };

    let mut writer = SliceWriter::new(slice);
    if build(inst, &mut writer).is_err() {
        return -1;
    }

    let len = writer.len();
    unsafe { *buf.add(len) = 0 };
    len as c_int
}

fn build(inst: &Instance, w: &mut SliceWriter) -> fmt::Result {
    write!(w, "{{\"v\":{VERSION},\"params\":{{")?;

    let mut first = true;
    for key in bridge::PARAM_KEYS {
        let mut scratch = Scratch::new();
        let Some(value) = bridge::get(inst, key, &mut scratch) else {
            continue;
        };
        if !first {
            w.write_char(',')?;
        }
        first = false;
        write_json_string(w, key)?;
        w.write_char(':')?;
        write_json_string(w, value)?;
    }
    w.write_str("}")?;

    // A preset carries its own tuning, so the scale text travels with it rather than the path it
    // was read from — a `.scl` is small and a tuning that silently reverts is worse than a big blob.
    if !inst.scala_scl.is_empty() || !inst.scala_kbm.is_empty() {
        w.write_str(",\"scala\":{\"scl\":")?;
        write_json_string(w, &inst.scala_scl)?;
        w.write_str(",\"kbm\":")?;
        write_json_string(w, &inst.scala_kbm)?;
        w.write_char('}')?;
    }

    write_sample(inst, w)?;
    w.write_char('}')
}

fn write_sample(inst: &Instance, w: &mut SliceWriter) -> fmt::Result {
    // Explicitly null rather than absent: a preset saved with no sample has to be able to say so,
    // and absence has to keep meaning "no opinion" for blobs written before this key existed.
    if inst.sample.is_empty() {
        return w.write_str(",\"sample\":null");
    }

    // Try to carry the audio. If it will not fit, rewind and say so in the blob rather than
    // guessing the size in advance and being wrong in the direction that loses everything.
    let mark = w.len();
    if sample_object(inst, w, true).is_ok() {
        return Ok(());
    }
    w.truncate(mark);
    sample_object(inst, w, false)
}

fn sample_object(inst: &Instance, w: &mut SliceWriter, embed: bool) -> fmt::Result {
    w.write_str(",\"sample\":{\"name\":")?;
    write_json_string(w, &inst.sample.name)?;
    w.write_str(",\"path\":")?;
    write_json_string(w, &inst.sample_path)?;
    write!(w, ",\"rate\":{}", inst.sample.source_rate)?;
    write!(w, ",\"embedded\":{embed}")?;

    if embed {
        w.write_str(",\"data\":\"")?;
        write_base64(w, inst.sample.data())?;
        w.write_char('"')?;
    }

    w.write_char('}')?;

    // The root object still has to close after this.
    if w.remaining() < TAIL_RESERVE {
        return Err(fmt::Error);
    }
    Ok(())
}

/// How long `data` bytes come out once base64'd. Only the tests need to predict this — the
/// writer finds out by trying, which is the point of [`SliceWriter::truncate`].
#[cfg(test)]
fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn write_base64(w: &mut SliceWriter, data: &[u8]) -> fmt::Result {
    for group in data.chunks(3) {
        let b0 = group[0] as u32;
        let b1 = group.get(1).copied().unwrap_or(0) as u32;
        let b2 = group.get(2).copied().unwrap_or(0) as u32;
        let packed = (b0 << 16) | (b1 << 8) | b2;

        let quad = [
            B64[(packed >> 18) as usize & 63],
            B64[(packed >> 12) as usize & 63],
            if group.len() > 1 {
                B64[(packed >> 6) as usize & 63]
            } else {
                b'='
            },
            if group.len() > 2 {
                B64[packed as usize & 63]
            } else {
                b'='
            },
        ];
        w.write_bytes(&quad)?;
    }
    Ok(())
}

/// Write a JSON string literal, escaping what RFC 8259 requires.
///
/// Scala text is multi-line and a sample path can hold anything a filesystem allows, so this has
/// to be real escaping rather than a pair of quotes.
fn write_json_string(w: &mut SliceWriter, text: &str) -> fmt::Result {
    w.write_char('"')?;
    for ch in text.chars() {
        match ch {
            '"' => w.write_str("\\\"")?,
            '\\' => w.write_str("\\\\")?,
            '\n' => w.write_str("\\n")?,
            '\r' => w.write_str("\\r")?,
            '\t' => w.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(w, "\\u{:04x}", c as u32)?,
            c => w.write_char(c)?,
        }
    }
    w.write_char('"')
}

// --- reading (slot load and preset recall; allocates) --------------------------------------------

/// Restore an instance from a blob. Silently ignores anything it does not recognise.
///
/// Not realtime-safe, and neither is the sample decode it ends in. This runs on the same path as
/// `load_sample` and carries the same caveat.
pub fn restore(inst: &mut Instance, blob: &str) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(blob) else {
        inst.error = "state blob is not JSON".into();
        return;
    };

    // Parameters first: the sample's root depends on `match_input_pitch` and `root_adjust`, which
    // are parameters, and loading the sample is what applies them.
    if let Some(params) = root.get("params").and_then(|p| p.as_object()) {
        for (key, value) in params {
            if let Some(text) = json_scalar(value) {
                bridge::set(inst, key, &text);
            }
        }
    }

    if let Some(scala) = root.get("scala") {
        let scl = scala.get("scl").and_then(|v| v.as_str()).unwrap_or("");
        let kbm = scala.get("kbm").and_then(|v| v.as_str()).unwrap_or("");
        inst.set_scala(scl, kbm);
    } else {
        inst.set_scala("", "");
    }

    restore_sample(inst, root.get("sample"));
}

fn restore_sample(inst: &mut Instance, sample: Option<&serde_json::Value>) {
    // No `sample` key at all means the blob has nothing to say about the sample — an older blob,
    // or one from something else — so leave whatever is loaded alone. Only an explicit null means
    // "this preset has no sample", which is what a blob written here says.
    //
    // Treating absence as a clear is how a slot that autosaved before its sample existed came back
    // silent on every reload, wiping the default the module had just loaded.
    let Some(sample) = sample else { return };
    if sample.is_null() {
        inst.clear_sample();
        return;
    }

    let path = sample.get("path").and_then(|v| v.as_str()).unwrap_or("");

    // Embedded audio wins over the path: it is the copy that definitely still exists, and it is
    // what makes a preset carried to another device sound the same.
    if let Some(encoded) = sample.get("data").and_then(|v| v.as_str()) {
        if let Some(data) = decode_base64(encoded) {
            let name = sample
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("sample");
            let rate = sample.get("rate").and_then(|v| v.as_u64()).unwrap_or(22050) as u32;
            inst.adopt_sample(SampleBuffer::new(name, data, rate), path);
            return;
        }
        inst.error = "state blob's audio would not decode".into();
    }

    if path.is_empty() {
        inst.clear_sample();
    } else {
        inst.load_sample(path);
    }
}

/// `module.json`'s `defaults` section: a flat object of parameter keys.
///
/// Runs once, inside `create_instance`, before the host has set anything.
pub fn apply_defaults(inst: &mut Instance, json: &str) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    let Some(object) = root.as_object() else {
        return;
    };

    for (key, value) in object {
        if let Some(text) = json_scalar(value) {
            bridge::set(inst, key, &text);
        }
    }
}

/// Flatten a JSON scalar to the string `set_param` expects.
///
/// A blob written by hand, or reformatted by a patch file, may hold `877` where we wrote `"877"`,
/// so both have to mean the same thing.
fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
        _ => None,
    }
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut packed = 0u32;
    let mut bits = 0u32;

    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            // Padding ends the stream; whitespace can appear if something pretty-printed the blob.
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };

        packed = (packed << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((packed >> bits) as u8);
        }
    }

    Some(out)
}

/// Serialise the sample browser's contents as the JSON array `items_param` levels expect.
///
/// The shadow UI parses this into `{index, label}` rows and sends back the index of whatever was
/// picked. Written straight into the host's buffer for the same reason the state blob is.
///
/// # Safety
///
/// `buf` must be writable for `buf_len` bytes, which is what `get_param` guarantees.
pub fn write_sample_list(inst: &Instance, buf: *mut c_char, buf_len: c_int) -> c_int {
    if buf.is_null() || buf_len <= 1 {
        return -1;
    }
    let capacity = buf_len as usize - 1;
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, capacity) };
    let mut w = SliceWriter::new(slice);

    let build = |w: &mut SliceWriter| -> fmt::Result {
        w.write_char('[')?;
        for (index, entry) in inst.samples.iter().enumerate() {
            if index > 0 {
                w.write_char(',')?;
            }
            write!(w, "{{\"index\":{index},\"label\":")?;
            // A marker on the loaded one, since the browser is the only place the current sample
            // is visible at all.
            if entry.path == inst.sample_path {
                let mut marked = String::with_capacity(entry.label.len() + 2);
                marked.push_str("* ");
                marked.push_str(&entry.label);
                write_json_string(w, &marked)?;
            } else {
                write_json_string(w, &entry.label)?;
            }
            w.write_char('}')?;
        }
        w.write_char(']')
    };

    if build(&mut w).is_err() {
        // Better an empty list than a truncated one the UI cannot parse.
        return write_empty_list(buf, capacity);
    }

    let len = w.len();
    unsafe { *buf.add(len) = 0 };
    len as c_int
}

fn write_empty_list(buf: *mut c_char, capacity: usize) -> c_int {
    if capacity < 3 {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(b"[]".as_ptr(), buf as *mut u8, 2);
        *buf.add(2) = 0;
    }
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(data: &[u8]) -> String {
        let mut buf = vec![0u8; base64_len(data.len()) + 8];
        let mut w = SliceWriter::new(&mut buf);
        write_base64(&mut w, data).unwrap();
        let len = w.len();
        String::from_utf8(buf[..len].to_vec()).unwrap()
    }

    #[test]
    fn base64_matches_the_known_answers() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_round_trips_every_byte_value() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(decode_base64(&encode(&data)).unwrap(), data);
    }

    #[test]
    fn base64_predicts_its_own_length() {
        for n in 0..64 {
            let data = vec![0xABu8; n];
            assert_eq!(encode(&data).len(), base64_len(n), "at {n} bytes");
        }
    }

    #[test]
    fn base64_survives_a_pretty_printed_blob() {
        assert_eq!(decode_base64("Zm9v\n  YmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("not base64!"), None);
    }

    #[test]
    fn json_strings_escape_what_would_break_the_blob() {
        let mut buf = vec![0u8; 128];
        let mut w = SliceWriter::new(&mut buf);
        write_json_string(&mut w, "a\"b\\c\nd\te").unwrap();
        let len = w.len();
        assert_eq!(
            std::str::from_utf8(&buf[..len]).unwrap(),
            r#""a\"b\\c\nd\te""#
        );
    }

    #[test]
    fn a_writer_that_runs_out_of_room_fails_rather_than_truncating() {
        let mut buf = vec![0u8; 4];
        let mut w = SliceWriter::new(&mut buf);
        assert!(w.write_str("12345").is_err());
        assert_eq!(w.len(), 0, "a rejected write must not be half-applied");
    }

    #[test]
    fn scalars_flatten_the_same_however_they_were_written() {
        assert_eq!(json_scalar(&serde_json::json!("877")).unwrap(), "877");
        assert_eq!(json_scalar(&serde_json::json!(877)).unwrap(), "877");
        assert_eq!(json_scalar(&serde_json::json!(true)).unwrap(), "1");
        assert_eq!(json_scalar(&serde_json::json!(null)), None);
    }
}
