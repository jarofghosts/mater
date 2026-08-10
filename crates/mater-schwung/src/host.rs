//! The host side of the ABI, transcribed from Schwung's `src/host/plugin_api_v1.h`.
//!
//! Field order and types here have to match that header exactly — the host hands over a raw
//! pointer and nothing checks the shape. Fields are read in declaration order, so anything
//! appended to the C struct later can be appended here without disturbing what came before.

use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::atomic::{AtomicPtr, Ordering};

pub type ModEmitValueFn = Option<
    unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        *const c_char,
        f32,
        f32,
        f32,
        c_int,
        c_int,
    ) -> c_int,
>;
pub type ModClearSourceFn = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

#[repr(C)]
pub struct HostApiV1 {
    pub api_version: u32,

    pub sample_rate: c_int,
    pub frames_per_block: c_int,

    pub mapped_memory: *mut u8,
    pub audio_out_offset: c_int,
    pub audio_in_offset: c_int,

    pub log: Option<unsafe extern "C" fn(*const c_char)>,

    pub midi_send_internal: Option<unsafe extern "C" fn(*const u8, c_int) -> c_int>,
    pub midi_send_external: Option<unsafe extern "C" fn(*const u8, c_int) -> c_int>,

    pub get_clock_status: Option<unsafe extern "C" fn() -> c_int>,

    pub mod_emit_value: ModEmitValueFn,
    pub mod_clear_source: ModClearSourceFn,
    pub mod_host_ctx: *mut c_void,

    pub get_bpm: Option<unsafe extern "C" fn() -> f32>,
    pub midi_inject_to_move: Option<unsafe extern "C" fn(*const u8, c_int) -> c_int>,
    pub slot_recv_channel: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,

    /// Appended in 2026-07. Older hosts leave this null — always guard.
    pub get_beat_position: Option<unsafe extern "C" fn() -> f64>,
}

/// The host pointer stays valid for the lifetime of the loaded library, so one global is enough
/// for every instance.
static HOST: AtomicPtr<HostApiV1> = AtomicPtr::new(std::ptr::null_mut());

pub fn store(host: *const HostApiV1) {
    HOST.store(host as *mut HostApiV1, Ordering::Release);
}

pub fn get() -> Option<&'static HostApiV1> {
    let ptr = HOST.load(Ordering::Acquire);
    // Safety: the host guarantees the struct outlives the plugin, and we only ever store what it
    // handed us in `move_plugin_init_v2`.
    unsafe { ptr.as_ref() }
}

/// Write a line to Schwung's unified log.
///
/// Allocates, so this is for load and teardown only. Never call it from `render_block`,
/// `on_midi` or `set_param` — all three run on the SPI/audio thread.
pub fn log(msg: &str) {
    let Some(host) = get() else { return };
    let Some(log) = host.log else { return };
    let Ok(text) = CString::new(format!("mater: {msg}")) else {
        return;
    };
    unsafe { log(text.as_ptr()) }
}

/// Current tempo, or 120 where the host does not report one.
pub fn bpm() -> f32 {
    match get().and_then(|h| h.get_bpm) {
        Some(get_bpm) => {
            let value = unsafe { get_bpm() };
            if value.is_finite() && value > 1.0 {
                value
            } else {
                120.0
            }
        }
        None => 120.0,
    }
}

/// Beats since transport start, or `None` when nothing is running.
pub fn beat_position() -> Option<f64> {
    let get_beat = get().and_then(|h| h.get_beat_position)?;
    let beats = unsafe { get_beat() };
    (beats >= 0.0 && beats.is_finite()).then_some(beats)
}
