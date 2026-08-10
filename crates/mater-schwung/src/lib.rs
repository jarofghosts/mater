//! mater as a Schwung module: the microGranny engine on Ableton Move hardware.
//!
//! This is the second wrapper around [`granny_core`]. The first is `mater-plugin`, which speaks
//! CLAP; this one speaks Schwung's `plugin_api_v2` (see `src/host/plugin_api_v1.h` in the Schwung
//! tree) so the engine can be loaded into a Signal Chain slot on a Move. Nothing from the CLAP
//! build comes along — no nih-plug, no egui, no symphonia — because the engine deliberately has no
//! host dependencies of its own.
//!
//! # Where this runs
//!
//! `render_block`, `on_midi`, `set_param` and `get_param` are **all called on the SPI/audio
//! thread**, at SCHED_FIFO with roughly 900 µs a frame. That is stricter than CLAP, where only
//! `process` is realtime. So:
//!
//! - nothing here allocates, logs or touches a file once an instance is up — including
//!   `get_param`, which the Shadow UI polls every tick, and [`state::write`], which formats a
//!   64 KB blob straight into the host's own buffer;
//! - the exceptions all read a file or parse JSON, and all sit on the slot-load path where a
//!   dropout is already expected: [`Instance::load_sample`], [`Instance::load_scala`] and
//!   [`state::restore`]. Moving the sample decode to a worker is phase 3's job.
//!
//! Every `extern "C"` entry point is wrapped in [`guard`]. A panic crossing the boundary aborts
//! the process, and on a Move that process is the audio server.
//!
//! # Scope
//!
//! Sound, the eight knobs, the setting bits, the mod matrix, MPE, Scala, the split, the hardware
//! CC map, and a state blob that carries all of it plus the sample. Not yet: anything drawn, and
//! anything beyond WAV. See `schwung/README.md`.

mod bridge;
pub use bridge::PARAM_KEYS;

pub mod host;
mod midi;
mod state;
mod wav;

use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::Path;
use std::sync::Arc;

use granny_core::scala::{KeyboardMap, ScalaScale, ScalaTuning};
use granny_core::{
    Engine, Fidelity, HwParams, ModSlot, MpeState, MpeZone, NoteMode, PitchTable, SampleBuffer,
    Scene, TransportInfo, Tuning, MOD_SLOTS,
};

use host::HostApiV1;

/// Move renders 128 frames a block. The scratch buffers are sized well past that so an unexpected
/// `frames` never has to allocate on the audio thread; anything larger is clamped instead.
pub const MAX_FRAMES: usize = 512;

/// The CLAP build offers sixteen. Measured on a Move this engine costs about 2.5 µs a voice a
/// block, which is nothing against a 2902 µs block — but the slack left after the SPI transfer
/// is only ~143 µs typically and was seen as low as 39 µs, shared with three other slots.
/// Sixteen would ask for 42 µs of that. `voices` raises it; see `schwung/README.md`.
const DEFAULT_VOICES: usize = 8;

const SAMPLE_RATE: f32 = 44_100.0;

/// 24 PPQN, as the transport reports it.
const TICKS_PER_BEAT: f64 = 24.0;

/// Where the sample browser looks, after the module's own directory.
///
/// `UserLibrary` is where Move keeps recordings and where Schwung's own resampler and skipback
/// write. Absolute, because a chain slot has no working directory worth speaking of, and skipped
/// silently when absent so the standalone host and a desktop test run are not full of errors.
const SAMPLE_ROOTS: &[&str] = &[
    "/data/UserData/UserLibrary/Samples",
    "/data/UserData/UserLibrary/Recordings",
];

/// How deep to walk, and how many entries to offer. Move's sample tree nests a few levels
/// (`Samples/Schwung/Resampler/<date>/`), and the list has to fit the host's 64 KB buffer with
/// room to spare.
const SAMPLE_SCAN_DEPTH: usize = 4;
const MAX_SAMPLES: usize = 256;
/// Longest label kept. The display is 128 px wide; anything past this is invisible anyway.
const MAX_LABEL: usize = 44;

/// One entry in the sample browser.
pub struct SampleEntry {
    pub path: String,
    /// What the browser shows: the path below its root, without the extension.
    pub label: String,
}

/// Recursively collect `.wav` files below `dir`, labelling each by its path under `base`.
fn collect_wavs(base: &Path, dir: &Path, depth: usize, out: &mut Vec<SampleEntry>) {
    if depth > SAMPLE_SCAN_DEPTH || out.len() >= MAX_SAMPLES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    // Sorted, so the list is stable between scans rather than in whatever order the filesystem
    // hands things back.
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if out.len() >= MAX_SAMPLES {
            return;
        }
        if path.is_dir() {
            collect_wavs(base, &path, depth + 1, out);
            continue;
        }
        let is_wav = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
        if !is_wav {
            continue;
        }

        let relative = path.strip_prefix(base).unwrap_or(&path);
        let mut label = relative.with_extension("").to_string_lossy().into_owned();
        if label.chars().count() > MAX_LABEL {
            // Keep the tail: the date directories Schwung writes share long prefixes, so the end
            // of the path is what distinguishes one recording from another.
            let skip = label.chars().count() - MAX_LABEL + 1;
            label = format!("…{}", label.chars().skip(skip).collect::<String>());
        }

        out.push(SampleEntry {
            path: path.to_string_lossy().into_owned(),
            label,
        });
    }
}

pub struct Instance {
    pub engine: Engine,
    pub params: HwParams,
    pub fidelity: Fidelity,
    pub tuning: Tuning,
    pub mods: [ModSlot; MOD_SLOTS],
    pub sample: SampleBuffer,

    /// Note Mode as the UI holds it: 0 Pitch, 1 Slice, 2 Split. Kept alongside
    /// [`HwParams::note_mode`] because the engine's `Split` carries its channel inside the variant,
    /// so leaving a split would otherwise forget which channel had been chosen.
    pub note_mode_index: u8,
    /// The split's slice channel, 0-based as MIDI delivers it. Held even when not splitting.
    pub slice_channel: u8,

    /// Per-channel bend, pressure and slide, and which of them a given channel's messages reach.
    pub mpe: MpeState,
    /// The zone the parameter asks for. What the state machine actually runs is this unless a
    /// split is in force — see [`Instance::sync_mpe`].
    pub mpe_zone: MpeZone,
    /// Bend range in semitones for member channels. The master's is [`Instance::bend_range`].
    pub mpe_bend_range: f32,
    /// Bend range for the master channel, and for everything when the zone is off.
    pub bend_range: f32,
    /// Whether RPN 0 may override either range.
    pub follow_rpn: bool,

    /// Whether the detected root is used, so reloading a sample re-applies the choice.
    pub match_input_pitch: bool,
    /// Hundredths of a semitone on top of detection, as the CLAP editor exposes it.
    pub root_adjust: f32,

    /// CC102–109 and CC1 driving the eight knobs, for an existing microGranny MIDI rig. Off by
    /// default, and an override rather than an automation — same caveat as the CLAP build.
    pub hardware_cc_map: bool,

    /// The Scala text, kept verbatim so the state blob can carry the tuning rather than a path to
    /// it. Empty means no scale is loaded.
    pub scala_scl: String,
    pub scala_kbm: String,

    pub module_dir: String,
    pub sample_path: String,
    /// Reported through `get_error`, which the host polls to show a module as failed.
    pub error: String,

    /// What the sample browser offers, cached. Built on first request rather than at load: the
    /// walk is file I/O, and paying it when someone opens the menu is better than paying it every
    /// time a slot loads.
    pub samples: Vec<SampleEntry>,
    pub samples_scanned: bool,

    left: Vec<f32>,
    right: Vec<f32>,

    /// Free-running position for when the host reports no transport, so grain sync still has
    /// something to advance against.
    beats: f64,
}

impl Instance {
    fn new(module_dir: &str) -> Self {
        let mut instance = Self {
            engine: Engine::new(DEFAULT_VOICES, SAMPLE_RATE),
            params: HwParams::default(),
            fidelity: Fidelity::default(),
            tuning: Tuning {
                // The CLAP build overrides the engine's default here for the reason the README
                // gives: the hardware table is up to +10 ¢ sharp, so it cannot track an incoming
                // note exactly. `pitch_table` puts the hardware's own tuning back.
                table: PitchTable::EqualTemperament,
                ..Tuning::default()
            },
            mods: [ModSlot::default(); MOD_SLOTS],
            sample: SampleBuffer::default(),
            note_mode_index: 0,
            slice_channel: 0,
            mpe: MpeState::default(),
            // The CLAP build defaults to the lower zone; here it defaults off. A Move slot
            // receives on one channel and forwards on one channel, so plain MIDI is the normal
            // case, and MPE needs the slot set to Receive=All + Forward=THRU before it means
            // anything at all. Defaulting to a zone would put a master channel where a Move track
            // simply is one.
            mpe_zone: MpeZone::Off,
            mpe_bend_range: granny_core::mpe::DEFAULT_MEMBER_BEND_RANGE,
            bend_range: granny_core::mpe::DEFAULT_MASTER_BEND_RANGE,
            follow_rpn: true,
            match_input_pitch: true,
            root_adjust: 0.0,
            hardware_cc_map: false,
            scala_scl: String::new(),
            scala_kbm: String::new(),
            module_dir: module_dir.to_string(),
            sample_path: String::new(),
            error: String::new(),
            left: vec![0.0; MAX_FRAMES],
            right: vec![0.0; MAX_FRAMES],
            beats: 0.0,
            samples: Vec::new(),
            samples_scanned: false,
        };

        instance.sync_mpe();

        // A module directory ships a sample, so a fresh slot makes a sound rather than looking
        // broken. Without one there is nothing to hear and no indication why.
        let default_sample = Path::new(module_dir).join("default.wav");
        if default_sample.is_file() {
            let path = default_sample.to_string_lossy().into_owned();
            instance.load_sample(&path);
        }

        instance
    }

    /// Decode a file and hand it to the engine.
    ///
    /// **This is not realtime-safe and `set_param` runs on the audio thread.** It reads a file,
    /// allocates, resamples, and runs YIN detection across the whole buffer, then drops the
    /// previous buffer. Expect a dropout on load. Phase 3 moves the work to a worker thread and
    /// swaps the buffer in through an atomic, with the old one freed off-thread; the shape of this
    /// function — decode, then assign, then re-derive the root — is what that has to preserve.
    pub fn load_sample(&mut self, path: &str) {
        let decoded = match wav::load(Path::new(path), &wav::LoadOptions::default()) {
            Ok(decoded) => decoded,
            Err(message) => {
                host::log(&format!("sample load failed: {message}"));
                self.error = message;
                return;
            }
        };

        let name = Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sample".to_string());

        self.adopt_sample(
            SampleBuffer::new(name, decoded.data, decoded.source_rate),
            path,
        );

        host::log(&format!(
            "loaded {} ({} bytes, root {:.2})",
            self.sample.name,
            self.sample.len(),
            self.tuning.root_note
        ));
    }

    /// Take an already-decoded buffer, as the state blob's embedded audio arrives.
    ///
    /// Every read head is pointing into the outgoing buffer, so the notes have to go first.
    pub fn adopt_sample(&mut self, sample: SampleBuffer, path: &str) {
        self.engine.all_notes_off();
        self.sample = sample;
        self.sample_path = path.to_string();
        self.error.clear();
        self.apply_root();
    }

    pub fn clear_sample(&mut self) {
        self.engine.all_notes_off();
        self.sample = SampleBuffer::default();
        self.sample_path.clear();
        self.apply_root();
    }

    /// Install a Scala scale and optional keyboard map from their text.
    ///
    /// Empty text removes the scale. A scale that will not parse is reported and leaves the
    /// previous tuning alone rather than dropping the instance into equal temperament mid-set.
    pub fn set_scala(&mut self, scl: &str, kbm: &str) {
        if scl.trim().is_empty() {
            self.tuning.scala = None;
            self.scala_scl.clear();
            self.scala_kbm.clear();
            return;
        }

        let scale = match ScalaScale::parse(scl) {
            Ok(scale) => scale,
            Err(error) => {
                self.error = format!("scale: {error:?}");
                return;
            }
        };

        let keymap = if kbm.trim().is_empty() {
            None
        } else {
            match KeyboardMap::parse(kbm) {
                Ok(map) => Some(map),
                Err(error) => {
                    self.error = format!("keyboard map: {error:?}");
                    return;
                }
            }
        };

        self.tuning.scala = Some(Arc::new(ScalaTuning::new(scale, keymap)));
        self.scala_scl = scl.to_string();
        self.scala_kbm = kbm.to_string();
        self.error.clear();
    }

    /// Walk the sample roots and cache what is there.
    ///
    /// Not realtime-safe — it is a directory walk. Called on the first `sample_list` request and
    /// on an explicit rescan, so the cost lands when someone opens the browser rather than on
    /// every slot load.
    pub fn scan_samples(&mut self) {
        self.samples.clear();
        self.samples_scanned = true;

        // The module's own directory first, so the sample it ships is the top of the list.
        let mut roots: Vec<String> = vec![self.module_dir.clone()];
        roots.extend(SAMPLE_ROOTS.iter().map(|r| r.to_string()));

        for root in &roots {
            let base = Path::new(root);
            if base.is_dir() {
                collect_wavs(base, base, 0, &mut self.samples);
            }
            if self.samples.len() >= MAX_SAMPLES {
                break;
            }
        }

        self.samples.sort_by(|a, b| a.label.cmp(&b.label));
        self.samples.truncate(MAX_SAMPLES);
    }

    /// Load the nth entry the browser offered.
    pub fn load_sample_index(&mut self, index: usize) {
        if !self.samples_scanned {
            self.scan_samples();
        }
        if let Some(entry) = self.samples.get(index) {
            let path = entry.path.clone();
            self.load_sample(&path);
        }
    }

    /// Load a `.scl` or `.kbm` off disk and install it. Not realtime-safe; see `load_sample`.
    pub fn load_scala(&mut self, path: &str, is_keymap: bool) {
        if path.trim().is_empty() {
            self.set_scala("", "");
            return;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                self.error = format!("{path}: {error}");
                return;
            }
        };
        if is_keymap {
            let scl = std::mem::take(&mut self.scala_scl);
            self.set_scala(&scl, &text);
        } else {
            let kbm = std::mem::take(&mut self.scala_kbm);
            self.set_scala(&text, &kbm);
        }
    }

    /// Rebuild the engine's note mode from the two controls the UI holds separately.
    pub fn apply_note_mode(&mut self) {
        self.params.note_mode = match self.note_mode_index {
            1 => NoteMode::Sliced,
            2 => NoteMode::Split {
                slice_channel: self.slice_channel,
            },
            _ => NoteMode::Tuned,
        };
        // A split changes whether the MPE zone may exist at all.
        self.sync_mpe();
    }

    /// The zone actually in force, which is not always the one the parameter asks for.
    ///
    /// A split routes by channel and an MPE zone hands every note a channel of its own; the two
    /// cannot both own the channel number, so the split wins and the input is read as plain MIDI.
    /// The CLAP build resolves this the same way, in its own `process`, and greys the MPE controls
    /// out to say so.
    pub fn effective_mpe_zone(&self) -> MpeZone {
        match self.params.note_mode {
            NoteMode::Split { .. } => MpeZone::Off,
            _ => self.mpe_zone,
        }
    }

    /// Push the MPE parameters into the state machine. Cheap, so it runs before every message.
    pub fn sync_mpe(&mut self) {
        let zone = self.effective_mpe_zone();
        self.mpe.set_zone(zone);
        self.mpe
            .set_ranges(self.mpe_bend_range, self.bend_range, self.follow_rpn);
    }

    /// Re-derive the tuning root from the loaded sample and the two controls over it.
    pub fn apply_root(&mut self) {
        let detected = self
            .match_input_pitch
            .then(|| self.sample.detected_root())
            .flatten();

        // Falling back to the engine's default is the hardware's behaviour: note 59 plays the file
        // at its recorded speed and the sounding pitch is whatever the sample happens to be.
        let base = detected.map_or(Tuning::default().root_note, |d| d.note);
        self.tuning.root_note = base + self.root_adjust / 100.0;
    }

    /// Where the transport is, without moving it. Note-on needs a tick but must not advance time.
    pub(crate) fn transport_snapshot(&self) -> TransportInfo {
        let ticks_per_sample = (host::bpm() as f64 / 60.0) * TICKS_PER_BEAT / SAMPLE_RATE as f64;
        let (playing, beats) = match host::beat_position() {
            Some(beats) => (true, beats),
            None => (false, self.beats),
        };
        TransportInfo {
            playing,
            pos_ticks: beats * TICKS_PER_BEAT,
            ticks_per_sample,
        }
    }

    /// The same, then advance the fallback clock by a block.
    fn transport(&mut self, frames: usize) -> TransportInfo {
        let info = self.transport_snapshot();
        match host::beat_position() {
            // Track the host so a stop-and-start does not jump.
            Some(beats) => self.beats = beats,
            // Nothing running: keep our own clock moving so a free-running grain still has a
            // monotonic tick to read against.
            None => self.beats += frames as f64 * info.ticks_per_sample / TICKS_PER_BEAT,
        }
        info
    }

    /// Render into Schwung's interleaved int16 mailbox.
    fn render(&mut self, out: &mut [i16]) {
        let frames = out.len() / 2;
        let transport = self.transport(frames);

        let left = &mut self.left[..frames];
        let right = &mut self.right[..frames];
        left.fill(0.0);
        right.fill(0.0);

        // An empty buffer is not silence to the engine — every read head would sit on a
        // zero-length file — so skip it entirely until something is loaded.
        if !self.sample.is_empty() {
            let scene = Scene {
                sample: &self.sample,
                params: &self.params,
                fidelity: &self.fidelity,
                tuning: &self.tuning,
                mods: &self.mods,
                transport,
            };
            self.engine.process(&scene, left, right);
        }

        // The engine queues ended voices for a host that wants to hear about them. Schwung has no
        // equivalent, but the queue still has to be emptied or it stops recording terminations.
        self.engine.drain_terminated(|_| {});

        for frame in 0..frames {
            out[frame * 2] = to_i16(self.left[frame]);
            out[frame * 2 + 1] = to_i16(self.right[frame]);
        }
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

// --- The C ABI ---------------------------------------------------------------------------------

/// Safety: the host only ever passes back a pointer it got from `create_instance`.
unsafe fn instance_mut<'a>(instance: *mut c_void) -> Option<&'a mut Instance> {
    (instance as *mut Instance).as_mut()
}

unsafe fn as_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    ptr.as_ref().and_then(|p| CStr::from_ptr(p).to_str().ok())
}

/// Run a body that must not unwind into C.
///
/// A panic crossing `extern "C"` aborts the process, and on a Move that process is the audio
/// server — a stray bounds check in a state parser would take the instrument down mid-set rather
/// than dropping one note. Nothing here is written to panic; this is the backstop for when that
/// turns out to be wrong.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// Copy a Rust string into the host's buffer as NUL-terminated C, returning the length written.
fn write_out(text: &str, buf: *mut c_char, buf_len: c_int) -> c_int {
    if buf.is_null() || buf_len <= 1 {
        return -1;
    }
    let capacity = buf_len as usize - 1;
    let bytes = text.as_bytes();
    let len = bytes.len().min(capacity);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, len);
        *buf.add(len) = 0;
    }
    len as c_int
}

unsafe extern "C" fn create_instance(
    module_dir: *const c_char,
    json_defaults: *const c_char,
) -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        let dir = as_str(module_dir).unwrap_or("");
        host::log(&format!("create_instance({dir})"));

        let mut instance = Instance::new(dir);
        // `module.json`'s `defaults` section, applied before anything else touches the instance.
        if let Some(defaults) = as_str(json_defaults) {
            state::apply_defaults(&mut instance, defaults);
        }
        Box::into_raw(Box::new(instance)) as *mut c_void
    })
}

unsafe extern "C" fn destroy_instance(instance: *mut c_void) {
    guard((), || {
        if instance.is_null() {
            return;
        }
        drop(Box::from_raw(instance as *mut Instance));
    })
}

unsafe extern "C" fn on_midi(instance: *mut c_void, msg: *const u8, len: c_int, _source: c_int) {
    guard((), || {
        let Some(inst) = instance_mut(instance) else {
            return;
        };
        if msg.is_null() || len <= 0 {
            return;
        }
        let bytes = std::slice::from_raw_parts(msg, len as usize);
        midi::handle(inst, bytes);
    })
}

unsafe extern "C" fn set_param(instance: *mut c_void, key: *const c_char, val: *const c_char) {
    guard((), || {
        let Some(inst) = instance_mut(instance) else {
            return;
        };
        let (Some(key), Some(val)) = (as_str(key), as_str(val)) else {
            return;
        };
        bridge::set(inst, key, val);
    })
}

unsafe extern "C" fn get_param(
    instance: *mut c_void,
    key: *const c_char,
    buf: *mut c_char,
    buf_len: c_int,
) -> c_int {
    guard(-1, || {
        let Some(inst) = instance_mut(instance) else {
            return -1;
        };
        let Some(key) = as_str(key) else { return -1 };

        // Both of these are too big for the scratch buffer and are written straight into the
        // host's, which is where the 64 KB ceiling actually lives.
        if key == "state" {
            return state::write(inst, buf, buf_len);
        }
        if key == "sample_list" {
            // Scanned on demand: this is the first moment we know someone wants the list.
            if !inst.samples_scanned {
                inst.scan_samples();
            }
            return state::write_sample_list(inst, buf, buf_len);
        }

        let mut scratch = bridge::Scratch::new();
        match bridge::get(inst, key, &mut scratch) {
            Some(value) => write_out(value, buf, buf_len),
            None => -1,
        }
    })
}

unsafe extern "C" fn get_error(instance: *mut c_void, buf: *mut c_char, buf_len: c_int) -> c_int {
    guard(0, || {
        let Some(inst) = instance_mut(instance) else {
            return 0;
        };
        if inst.error.is_empty() {
            return 0;
        }
        write_out(&inst.error, buf, buf_len).max(0)
    })
}

unsafe extern "C" fn render_block(instance: *mut c_void, out: *mut i16, frames: c_int) {
    guard((), || {
        let Some(inst) = instance_mut(instance) else {
            return;
        };
        if out.is_null() || frames <= 0 {
            return;
        }
        let frames = (frames as usize).min(MAX_FRAMES);
        let buffer = std::slice::from_raw_parts_mut(out, frames * 2);
        inst.render(buffer);
    })
}

#[repr(C)]
pub struct PluginApiV2 {
    pub api_version: u32,
    pub create_instance: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void,
    pub destroy_instance: unsafe extern "C" fn(*mut c_void),
    pub on_midi: unsafe extern "C" fn(*mut c_void, *const u8, c_int, c_int),
    pub set_param: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char),
    pub get_param: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, c_int) -> c_int,
    pub get_error: unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int,
    pub render_block: unsafe extern "C" fn(*mut c_void, *mut i16, c_int),
}

// Safety: this is a table of function pointers and a constant. It is never mutated after the
// static is laid down, and the host only reads it.
unsafe impl Sync for PluginApiV2 {}

static API: PluginApiV2 = PluginApiV2 {
    api_version: 2,
    create_instance,
    destroy_instance,
    on_midi,
    set_param,
    get_param,
    get_error,
    render_block,
};

/// The symbol the Schwung host dlsym()s. Chain sound generators are loaded as `dsp.so`.
///
/// # Safety
///
/// `host_api` must be a valid `host_api_v1_t` that outlives the loaded library, which is what
/// `module_manager.c` and `chain_host.c` both guarantee. Null is accepted: the module then runs
/// without logging or transport, which is what the standalone host does.
#[no_mangle]
pub unsafe extern "C" fn move_plugin_init_v2(host_api: *const HostApiV1) -> *const PluginApiV2 {
    guard(&API as *const PluginApiV2, || {
        host::store(host_api);
        host::log(concat!("mater ", env!("CARGO_PKG_VERSION"), " ready"));
        &API as *const PluginApiV2
    })
}
