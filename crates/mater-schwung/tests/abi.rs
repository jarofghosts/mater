//! Drive the module through the same table the Schwung host does.
//!
//! Everything here goes through the raw `plugin_api_v2` function pointers rather than calling
//! `Instance` directly, because the ABI is the part that has no compiler checking it: a wrong
//! signature or a mishandled null shows up as a segfault on the device and as nothing at all in a
//! unit test.
//!
//! The host pointer is null throughout, which is the case a plugin has to survive anyway — an
//! older host leaves `get_beat_position` unset, and the standalone host passes no tempo at all.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;

use mater_schwung::{move_plugin_init_v2, PluginApiV2};

const FRAMES: usize = 128;

fn api() -> &'static PluginApiV2 {
    unsafe { &*move_plugin_init_v2(std::ptr::null()) }
}

fn create(api: &PluginApiV2) -> *mut c_void {
    let dir = CString::new("").unwrap();
    let instance = unsafe { (api.create_instance)(dir.as_ptr(), std::ptr::null()) };
    assert!(!instance.is_null(), "create_instance returned null");
    instance
}

fn set(api: &PluginApiV2, instance: *mut c_void, key: &str, val: &str) {
    let key = CString::new(key).unwrap();
    let val = CString::new(val).unwrap();
    unsafe { (api.set_param)(instance, key.as_ptr(), val.as_ptr()) }
}

fn get(api: &PluginApiV2, instance: *mut c_void, key: &str) -> Option<String> {
    let key = CString::new(key).unwrap();
    let mut buf = vec![0 as c_char; 4096];
    let len =
        unsafe { (api.get_param)(instance, key.as_ptr(), buf.as_mut_ptr(), buf.len() as i32) };
    if len < 0 {
        return None;
    }
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        text.len(),
        len as usize,
        "{key:?} reported the wrong length"
    );
    Some(text)
}

fn midi(api: &PluginApiV2, instance: *mut c_void, bytes: [u8; 3]) {
    unsafe { (api.on_midi)(instance, bytes.as_ptr(), 3, 0) }
}

/// Render `blocks` blocks and return the loudest sample seen.
fn render_peak(api: &PluginApiV2, instance: *mut c_void, blocks: usize) -> i16 {
    let mut peak = 0i16;
    let mut out = vec![0i16; FRAMES * 2];
    for _ in 0..blocks {
        out.fill(0);
        unsafe { (api.render_block)(instance, out.as_mut_ptr(), FRAMES as i32) }
        peak = peak.max(out.iter().map(|s| s.saturating_abs()).max().unwrap_or(0));
    }
    peak
}

/// One second of a 220 Hz sine at 22050 Hz, mono 16-bit, written where the module can load it.
fn write_test_wav(name: &str) -> PathBuf {
    write_test_wav_seconds(name, 1)
}

/// The same, at a chosen length — which is what decides whether the state blob can carry it.
fn write_test_wav_seconds(name: &str, seconds: u32) -> PathBuf {
    let rate = 22050u32;
    let mut samples = Vec::new();
    for i in 0..rate * seconds {
        let phase = i as f32 / rate as f32 * 220.0 * std::f32::consts::TAU;
        samples.extend_from_slice(&((phase.sin() * 20000.0) as i16).to_le_bytes());
    }

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(&samples);

    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, wav).unwrap();
    path
}

#[test]
fn the_entry_point_reports_version_two() {
    assert_eq!(api().api_version, 2);
}

#[test]
fn a_note_on_a_loaded_sample_makes_sound() {
    let api = api();
    let instance = create(api);
    let path = write_test_wav("mater-abi-sound.wav");

    // Nothing loaded yet, so nothing should come out however hard it is played.
    midi(api, instance, [0x90, 60, 100]);
    assert_eq!(
        render_peak(api, instance, 8),
        0,
        "silent before a sample loads"
    );

    set(api, instance, "sample_path", path.to_str().unwrap());
    assert_eq!(
        get(api, instance, "sample_name").as_deref(),
        Some("mater-abi-sound")
    );

    // A 220 Hz sine is A3, so detection should land on note 57 and the root follow it.
    let root: f32 = get(api, instance, "sample_root").unwrap().parse().unwrap();
    assert!((root - 57.0).abs() < 0.5, "detected root was {root}");

    midi(api, instance, [0x90, 60, 100]);
    assert!(
        render_peak(api, instance, 64) > 0,
        "a held note produced silence"
    );

    midi(api, instance, [0x80, 60, 0]);
    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn parameters_round_trip_through_the_string_interface() {
    let api = api();
    let instance = create(api);

    for (key, sent, expected) in [
        ("rate", "500", "500"),
        ("grain", "40", "40"),
        ("shift", "200", "200"),
        ("note_mode", "1", "1"),
        ("repeat", "0", "0"),
        ("interpolate", "1", "1"),
        // Out of range in both directions: clamped, never rejected.
        ("crush", "999", "127"),
        ("start", "-1", "0"),
        // Garbage leaves the previous value alone rather than zeroing it.
        ("end", "banana", "1022"),
    ] {
        set(api, instance, key, sent);
        assert_eq!(get(api, instance, key).as_deref(), Some(expected), "{key}");
    }

    assert_eq!(get(api, instance, "no_such_param"), None);
    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn chain_params_covers_every_key_the_ui_can_reach() {
    let api = api();
    let instance = create(api);

    let json = get(api, instance, "chain_params").expect("chain_params is required");
    assert!(json.starts_with('[') && json.ends_with(']'));

    // Every advertised key has to answer get_param, or the Shadow UI draws a blank row.
    for key in json.split("\"key\":\"").skip(1) {
        let key = &key[..key.find('"').unwrap()];
        assert!(
            get(api, instance, key).is_some(),
            "{key} is advertised but unreadable"
        );
    }

    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn a_missing_sample_is_reported_rather_than_fatal() {
    let api = api();
    let instance = create(api);

    set(api, instance, "sample_path", "/nowhere/at/all.wav");

    let mut buf = vec![0 as c_char; 256];
    let len = unsafe { (api.get_error)(instance, buf.as_mut_ptr(), buf.len() as i32) };
    assert!(len > 0, "a failed load should leave an error behind");

    // And it still renders, silently, instead of taking the audio thread down with it.
    assert_eq!(render_peak(api, instance, 4), 0);
    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn the_null_cases_the_host_can_hand_us_are_survivable() {
    let api = api();

    // Every entry point is reached with a null instance at least once during teardown races.
    unsafe {
        (api.on_midi)(std::ptr::null_mut(), [0x90, 60, 100].as_ptr(), 3, 0);
        (api.render_block)(std::ptr::null_mut(), std::ptr::null_mut(), 128);
        (api.destroy_instance)(std::ptr::null_mut());
    }

    let instance = create(api);
    unsafe {
        // A null buffer, and a zero-length message.
        (api.render_block)(instance, std::ptr::null_mut(), 128);
        (api.on_midi)(instance, [].as_ptr(), 0, 0);
        (api.set_param)(instance, std::ptr::null(), std::ptr::null());
        (api.destroy_instance)(instance);
    }
}

#[test]
fn a_block_larger_than_the_scratch_buffers_is_clamped_not_overrun() {
    let api = api();
    let instance = create(api);

    // The host says 128. If something ever says more, the buffers must not be outgrown.
    let mut out = vec![0i16; (mater_schwung::MAX_FRAMES + 512) * 2];
    unsafe {
        (api.render_block)(
            instance,
            out.as_mut_ptr(),
            (mater_schwung::MAX_FRAMES + 512) as i32,
        )
    }

    unsafe { (api.destroy_instance)(instance) }
}

// --- phase 2: state, MPE, mod matrix, the hardware CC map ----------------------------------------

/// `get_param("state")` needs the host's full 64 KB, not the 4 KB the other keys fit in.
fn get_state(api: &PluginApiV2, instance: *mut c_void) -> String {
    let key = CString::new("state").unwrap();
    let mut buf = vec![0 as c_char; 65536];
    let len =
        unsafe { (api.get_param)(instance, key.as_ptr(), buf.as_mut_ptr(), buf.len() as i32) };
    assert!(
        len > 0,
        "state must never come back empty — Schwung's autosave bails if it does"
    );
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(text.len(), len as usize);
    text
}

/// A `.scl` small enough to travel inside the blob, which is the point of carrying the text.
fn write_test_scale(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(
        &path,
        "! test.scl\n!\nEqual-ish quarter tones\n 2\n!\n 150.0\n 2/1\n",
    )
    .unwrap();
    path
}

#[test]
fn state_round_trips_every_parameter() {
    let api = api();
    let source = create(api);

    // Move every parameter off its default, so a value that failed to travel shows up as the
    // default rather than hiding behind one.
    let changed = [
        ("rate", "500"),
        ("crush", "9"),
        ("attack", "11"),
        ("release", "12"),
        ("grain", "40"),
        ("shift", "200"),
        ("start", "64"),
        ("end", "900"),
        ("note_mode", "2"),
        ("slice_channel", "7"),
        ("level", "0.800"),
        ("vel_sensitivity", "0.500"),
        ("legato", "1"),
        ("repeat", "0"),
        ("sync", "0"),
        ("random_shift", "1"),
        ("hold", "1"),
        ("voices", "5"),
        ("pitch_table", "1"),
        ("snap", "2"),
        ("match_input_pitch", "0"),
        ("root_adjust", "150"),
        ("mpe_zone", "2"),
        ("mpe_bend_range", "24"),
        ("bend_range", "12"),
        ("follow_rpn", "0"),
        ("hardware_cc_map", "1"),
        ("mod1_source", "1"),
        ("mod1_dest", "5"),
        ("mod1_depth", "0.50"),
        ("mod2_source", "4"),
        ("mod2_dest", "9"),
        ("mod2_depth", "-0.25"),
        ("mod3_source", "5"),
        ("mod3_dest", "6"),
        ("mod3_depth", "1.00"),
        ("curve_mode", "1"),
        ("interpolate", "1"),
        ("quantize_seeks", "0"),
        ("grain_fade_ms", "2.5"),
    ];
    assert_eq!(
        changed.len(),
        mater_schwung::PARAM_KEYS.len(),
        "a parameter was added without being covered here"
    );
    for (key, value) in changed {
        set(api, source, key, value);
    }

    let blob = get_state(api, source);
    assert!(
        serde_json::from_str::<serde_json::Value>(&blob).is_ok(),
        "state must be JSON"
    );

    let restored = create(api);
    set(api, restored, "state", &blob);

    for (key, expected) in changed {
        assert_eq!(
            get(api, restored, key).as_deref(),
            Some(expected),
            "{key} did not survive"
        );
    }

    unsafe {
        (api.destroy_instance)(source);
        (api.destroy_instance)(restored);
    }
}

#[test]
fn a_short_sample_rides_inside_the_blob() {
    let api = api();
    let source = create(api);
    let path = write_test_wav("mater-abi-embed.wav");

    set(api, source, "sample_path", path.to_str().unwrap());
    let blob = get_state(api, source);

    let parsed: serde_json::Value = serde_json::from_str(&blob).unwrap();
    assert_eq!(parsed["sample"]["embedded"], serde_json::json!(true));
    assert!(parsed["sample"]["data"].is_string());

    // Take the file away. A self-contained preset has to survive exactly this.
    std::fs::remove_file(&path).unwrap();

    let restored = create(api);
    set(api, restored, "state", &blob);
    assert_eq!(
        get(api, restored, "sample_name").as_deref(),
        Some("mater-abi-embed")
    );
    assert_eq!(
        get(api, restored, "sample_frames"),
        get(api, source, "sample_frames"),
        "the embedded audio must be the same length as what was captured"
    );

    midi(api, restored, [0x90, 60, 100]);
    assert!(
        render_peak(api, restored, 64) > 0,
        "a restored preset produced silence"
    );

    unsafe {
        (api.destroy_instance)(source);
        (api.destroy_instance)(restored);
    }
}

#[test]
fn a_sample_too_big_for_the_blob_falls_back_to_its_path() {
    let api = api();
    let source = create(api);
    // Four seconds is ~88 KB of 8-bit audio, ~118 KB base64'd — well past the host's 64 KB.
    let path = write_test_wav_seconds("mater-abi-toobig.wav", 4);

    set(api, source, "sample_path", path.to_str().unwrap());
    let blob = get_state(api, source);

    let parsed: serde_json::Value = serde_json::from_str(&blob).unwrap();
    assert_eq!(
        parsed["sample"]["embedded"],
        serde_json::json!(false),
        "the blob has to admit when the audio is by reference"
    );
    assert!(parsed["sample"]["data"].is_null());
    assert_eq!(
        parsed["sample"]["path"],
        serde_json::json!(path.to_str().unwrap())
    );
    assert!(blob.len() < 65536, "state overran the host's buffer");

    // The file is still there, so recall works — it just went through the path.
    let restored = create(api);
    set(api, restored, "state", &blob);
    assert_eq!(
        get(api, restored, "sample_name").as_deref(),
        Some("mater-abi-toobig")
    );

    unsafe {
        (api.destroy_instance)(source);
        (api.destroy_instance)(restored);
    }
}

#[test]
fn a_scale_travels_with_the_preset() {
    let api = api();
    let source = create(api);
    let path = write_test_scale("mater-abi.scl");

    set(api, source, "scala_path", path.to_str().unwrap());
    assert_eq!(
        get(api, source, "scala_name").as_deref(),
        Some("Equal-ish quarter tones")
    );

    let blob = get_state(api, source);
    std::fs::remove_file(&path).unwrap();

    let restored = create(api);
    set(api, restored, "state", &blob);
    assert_eq!(
        get(api, restored, "scala_name").as_deref(),
        Some("Equal-ish quarter tones"),
        "the scale text must travel, not the path it was read from"
    );

    unsafe {
        (api.destroy_instance)(source);
        (api.destroy_instance)(restored);
    }
}

#[test]
fn a_split_forces_the_mpe_zone_off() {
    let api = api();
    let instance = create(api);

    set(api, instance, "mpe_zone", "1");
    assert_eq!(get(api, instance, "mpe_active").as_deref(), Some("1"));

    // A split routes by channel and a zone hands every note a channel of its own. They cannot both
    // own the channel number, so the split wins — but the parameter must remember what was asked
    // for, or leaving the split would silently lose the zone.
    set(api, instance, "note_mode", "2");
    assert_eq!(get(api, instance, "mpe_active").as_deref(), Some("0"));
    assert_eq!(get(api, instance, "mpe_zone").as_deref(), Some("1"));

    set(api, instance, "note_mode", "0");
    assert_eq!(get(api, instance, "mpe_active").as_deref(), Some("1"));

    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn the_slice_channel_survives_leaving_and_re_entering_a_split() {
    let api = api();
    let instance = create(api);

    set(api, instance, "slice_channel", "9");
    set(api, instance, "note_mode", "2");
    set(api, instance, "note_mode", "0");
    set(api, instance, "note_mode", "2");
    assert_eq!(get(api, instance, "slice_channel").as_deref(), Some("9"));

    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn the_hardware_cc_map_moves_the_knobs_only_once_it_is_on() {
    let api = api();
    let instance = create(api);

    // CC102 is the first of the eight. Off by default, so it should do nothing at all.
    midi(api, instance, [0xB0, 102, 64]);
    assert_eq!(
        get(api, instance, "rate").as_deref(),
        Some("877"),
        "the map is off by default"
    );

    set(api, instance, "hardware_cc_map", "1");
    midi(api, instance, [0xB0, 102, 64]);
    // The firmware scales the 7-bit CC up to each knob's own bit depth: 64/127 of 1023 is 515.5.
    assert_eq!(get(api, instance, "rate").as_deref(), Some("516"));

    // Modwheel is wired to crush on the 2.5.
    midi(api, instance, [0xB0, 1, 127]);
    assert_eq!(get(api, instance, "crush").as_deref(), Some("127"));

    // And unlike the CLAP build, the displayed parameter moves with it — Schwung has no
    // audio-thread restriction on a module writing its own parameters.
    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn module_json_defaults_are_applied_at_creation() {
    let api = api();
    let dir = CString::new("").unwrap();
    let defaults = CString::new(r#"{"grain":"40","crush":7,"legato":true}"#).unwrap();
    let instance = unsafe { (api.create_instance)(dir.as_ptr(), defaults.as_ptr()) };
    assert!(!instance.is_null());

    // Written three different ways — a string, a number and a bool — because a hand-edited
    // module.json will use whichever looks natural.
    assert_eq!(get(api, instance, "grain").as_deref(), Some("40"));
    assert_eq!(get(api, instance, "crush").as_deref(), Some("7"));
    assert_eq!(get(api, instance, "legato").as_deref(), Some("1"));

    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn a_state_blob_that_is_not_ours_is_survivable() {
    let api = api();

    for blob in [
        "",
        "not json at all",
        "[]",
        "{}",
        r#"{"params":null,"sample":42}"#,
    ] {
        let instance = create(api);
        set(api, instance, "state", blob);
        // No panic across the ABI, and it still renders.
        assert_eq!(render_peak(api, instance, 4), 0);
        unsafe { (api.destroy_instance)(instance) }
    }
}

#[test]
fn the_sample_browser_lists_and_loads() {
    let api = api();

    // A module directory standing in for an installed one: the browser lists its own directory
    // first, which is how the shipped `default.wav` reaches the top of the list.
    let dir = std::env::temp_dir().join("mater-abi-browser");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("kits")).unwrap();
    std::fs::copy(write_test_wav("mater-abi-b1.wav"), dir.join("alpha.wav")).unwrap();
    std::fs::copy(
        write_test_wav("mater-abi-b2.wav"),
        dir.join("kits/beta.wav"),
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), b"not audio").unwrap();

    let module_dir = CString::new(dir.to_str().unwrap()).unwrap();
    let instance = unsafe { (api.create_instance)(module_dir.as_ptr(), std::ptr::null()) };
    assert!(!instance.is_null());

    let listing = get(api, instance, "sample_list").expect("sample_list is required");
    let items: serde_json::Value = serde_json::from_str(&listing).unwrap();
    let items = items.as_array().unwrap();

    let labels: Vec<&str> = items
        .iter()
        .map(|i| i["label"].as_str().unwrap().trim_start_matches("* "))
        .collect();
    assert!(labels.contains(&"alpha"), "got {labels:?}");
    assert!(
        labels.contains(&"kits/beta"),
        "a nested sample, labelled by its path: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l.contains("notes")),
        "non-audio must not be offered"
    );

    // Every row carries the index the UI sends back.
    for (n, item) in items.iter().enumerate() {
        assert_eq!(item["index"], serde_json::json!(n));
    }

    // Pick the nested one by its index and confirm that is what loaded.
    let beta = labels.iter().position(|l| *l == "kits/beta").unwrap();
    set(api, instance, "sample_index", &beta.to_string());
    // The name comes from the file that loaded, not from whatever it was copied from.
    assert_eq!(get(api, instance, "sample_name").as_deref(), Some("beta"));

    midi(api, instance, [0x90, 60, 100]);
    assert!(
        render_peak(api, instance, 64) > 0,
        "the picked sample produced silence"
    );

    // The loaded row is marked, because the browser is the only place the current sample shows.
    let listing = get(api, instance, "sample_list").unwrap();
    let items: serde_json::Value = serde_json::from_str(&listing).unwrap();
    assert!(
        items.as_array().unwrap()[beta]["label"]
            .as_str()
            .unwrap()
            .starts_with("* "),
        "the loaded sample should be marked"
    );

    unsafe { (api.destroy_instance)(instance) }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_shipped_default_sample_makes_a_fresh_slot_audible() {
    let api = api();

    // The module ships one so a freshly-loaded slot is not silent with no way to tell why.
    let dir = std::env::temp_dir().join("mater-abi-default");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(write_test_wav("mater-abi-def.wav"), dir.join("default.wav")).unwrap();

    let module_dir = CString::new(dir.to_str().unwrap()).unwrap();
    let instance = unsafe { (api.create_instance)(module_dir.as_ptr(), std::ptr::null()) };

    assert_eq!(
        get(api, instance, "sample_name").as_deref(),
        Some("default")
    );
    midi(api, instance, [0x90, 60, 100]);
    assert!(
        render_peak(api, instance, 64) > 0,
        "a fresh slot must make a sound"
    );

    unsafe { (api.destroy_instance)(instance) }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_hierarchy_is_served_to_the_chain_host() {
    let api = api();
    let instance = create(api);

    // The chain host forwards `synth:ui_hierarchy` straight to the plugin — there is no
    // module.json fallback for a synth. Returning nothing here is a slot with no menu.
    let json = get(api, instance, "ui_hierarchy").expect("a chainable synth must serve this");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let levels = parsed["levels"].as_object().expect("levels");

    assert!(levels.contains_key("root"));
    let sample = levels
        .get("sample")
        .expect("the sample browser must be reachable");
    assert_eq!(sample["items_param"], serde_json::json!("sample_list"));

    let root_rows = levels["root"]["params"].as_array().unwrap();
    assert!(
        root_rows
            .iter()
            .any(|r| r.get("level").and_then(|l| l.as_str()) == Some("sample")),
        "nothing at the root navigates to the sample browser"
    );

    unsafe { (api.destroy_instance)(instance) }
}

#[test]
fn a_blob_with_no_opinion_on_the_sample_leaves_it_loaded() {
    let api = api();

    // A slot that autosaved before its sample existed came back silent forever after: the blob had
    // no `sample` key, restore read that as "clear", and it wiped the default the module had just
    // loaded on every single reload.
    let dir = std::env::temp_dir().join("mater-abi-noopinion");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        write_test_wav("mater-abi-noop.wav"),
        dir.join("default.wav"),
    )
    .unwrap();
    let module_dir = CString::new(dir.to_str().unwrap()).unwrap();

    let instance = unsafe { (api.create_instance)(module_dir.as_ptr(), std::ptr::null()) };
    assert_eq!(
        get(api, instance, "sample_name").as_deref(),
        Some("default")
    );

    set(api, instance, "state", r#"{"v":1,"params":{"grain":"40"}}"#);
    assert_eq!(
        get(api, instance, "grain").as_deref(),
        Some("40"),
        "params still apply"
    );
    assert_eq!(
        get(api, instance, "sample_name").as_deref(),
        Some("default"),
        "a blob that says nothing about the sample must not clear it"
    );
    midi(api, instance, [0x90, 60, 100]);
    assert!(render_peak(api, instance, 64) > 0);

    // An explicit null is how a preset says it genuinely has no sample.
    set(
        api,
        instance,
        "state",
        r#"{"v":1,"params":{},"sample":null}"#,
    );
    assert_eq!(get(api, instance, "sample_frames").as_deref(), Some("0"));

    unsafe { (api.destroy_instance)(instance) }
    let _ = std::fs::remove_dir_all(&dir);
}
