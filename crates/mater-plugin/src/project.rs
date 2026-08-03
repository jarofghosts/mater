//! Project files: one `.mater` on disk holding the sample, the tuning and every setting.
//!
//! The plugin's state already carries all of that — a host saving its own project writes exactly
//! the same thing into its own file. This module puts it somewhere you can name, so a setup can
//! move between projects and between hosts, and so the standalone build has somewhere to keep one.
//!
//! The payload is nih-plug's own [`PluginState`] rather than a struct of our own. That is the whole
//! point: a project cannot drift out of step with what the plugin actually persists, because adding
//! a parameter or a persisted field adds it to both at once.

use nih_plug::prelude::PluginState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The extension a project is saved under.
pub const EXTENSION: &str = "mater";

/// Written into every file so we can tell a project from any other JSON before interpreting it.
const FORMAT: &str = "mater-project";

/// Bumped only if the envelope around the state changes shape. The state inside it carries its own
/// version, which is what nih-plug's `filter_state` would migrate.
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Project {
    format: String,
    format_version: u32,
    /// Which build wrote it. Informational — the state has its own version for migrations.
    written_by: String,
    state: PluginState,
}

/// Encode a plugin state as the bytes of a project file.
pub fn encode(state: PluginState) -> Result<Vec<u8>, String> {
    let project = Project {
        format: FORMAT.to_string(),
        format_version: FORMAT_VERSION,
        written_by: env!("CARGO_PKG_VERSION").to_string(),
        state,
    };
    // Pretty-printed: the sample is one long base64 line either way, and everything else being
    // legible makes a project something you can open and read.
    serde_json::to_vec_pretty(&project).map_err(|e| format!("could not encode project: {e}"))
}

/// Decode a project file, or explain why it is not one.
pub fn decode(bytes: &[u8]) -> Result<PluginState, String> {
    let project: Project =
        serde_json::from_slice(bytes).map_err(|e| format!("not a readable project file: {e}"))?;

    if project.format != FORMAT {
        return Err(format!(
            "not a mater project — it calls itself {:?}",
            project.format
        ));
    }
    if project.format_version > FORMAT_VERSION {
        return Err(format!(
            "written by a newer mater (project format {}, this build reads {FORMAT_VERSION})",
            project.format_version
        ));
    }

    Ok(project.state)
}

/// Write a project, returning the path it actually landed on.
pub fn save(path: &Path, state: PluginState) -> Result<PathBuf, String> {
    let path = with_extension(path);
    let bytes = encode(state)?;
    std::fs::write(&path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// Read a project back.
pub fn load(path: &Path) -> Result<PluginState, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot open file: {e}"))?;
    decode(&bytes)
}

/// Make sure a saved file is named `.mater`, since not every dialog appends the filter's extension.
///
/// It appends rather than replaces: a name typed as `take.3` should not become `take.mater` and
/// quietly overwrite something else.
fn with_extension(path: &Path) -> PathBuf {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
    {
        return path.to_path_buf();
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(EXTENSION);
    PathBuf::from(name)
}

/// What to offer a new project file as a name: whatever the sample is called, since that is what
/// identifies the preset.
pub fn default_file_name(sample_name: &str) -> String {
    let stem = Path::new(sample_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let stem = if stem.is_empty() { "untitled" } else { stem };
    format!("{stem}.{EXTENSION}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nih_plug::wrapper::state::ParamValue;
    use std::collections::BTreeMap;

    fn state() -> PluginState {
        PluginState {
            version: "0.1.0".into(),
            params: BTreeMap::from([
                ("rate".to_string(), ParamValue::I32(512)),
                ("legato".to_string(), ParamValue::Bool(true)),
                (
                    "notemode".to_string(),
                    ParamValue::String("slice".to_string()),
                ),
            ]),
            fields: BTreeMap::from([("sample".to_string(), r#"{"name":"kick.wav"}"#.to_string())]),
        }
    }

    #[test]
    fn a_project_round_trips_its_parameters_and_its_fields() {
        let encoded = encode(state()).unwrap();
        let decoded = decode(&encoded).unwrap();

        assert!(matches!(decoded.params["rate"], ParamValue::I32(512)));
        assert!(matches!(decoded.params["legato"], ParamValue::Bool(true)));
        assert!(
            matches!(&decoded.params["notemode"], ParamValue::String(id) if id == "slice"),
            "enum variants stay stable ids, not indices"
        );
        assert_eq!(decoded.fields["sample"], r#"{"name":"kick.wav"}"#);
    }

    /// The one that matters: a real parameter set, with a sample and a tuning in it, written out
    /// and read back into a fresh instance — and the sample published to the audio thread on the
    /// way in, which is what makes a restored project audible rather than merely correct on paper.
    #[test]
    fn a_sample_and_its_tuning_come_back_from_a_project_file() {
        use crate::loader::{StoredSample, StoredScale};
        use crate::params::MaterParams;
        use crate::shared::Shared;
        use nih_plug::params::Params;
        use std::sync::Arc;

        let saved = MaterParams::new(Arc::new(Shared::default()));
        saved.sample.store(StoredSample {
            name: "kick.wav".into(),
            path: "/tmp/kick.wav".into(),
            source_rate: 22050,
            data: (0..2048).map(|i| (i % 256) as u8).collect(),
        });
        saved
            .scale
            .store(StoredScale {
                scl_name: "test.scl".into(),
                scl: "!\ntest\n 3\n 3/2\n 2\n 1200.0\n".into(),
                ..Default::default()
            })
            .unwrap();
        saved.set_ui_scale(1.75);

        let written = encode(PluginState {
            version: "0.1.0".into(),
            params: BTreeMap::new(),
            fields: saved.serialize_fields(),
        })
        .unwrap();

        let shared = Arc::new(Shared::default());
        let restored = MaterParams::new(shared.clone());
        restored.deserialize_fields(&decode(&written).unwrap().fields);

        let sample = shared.editor_sample.lock().clone();
        assert_eq!(sample.name, "kick.wav");
        assert_eq!(sample.len(), 2048);
        assert_eq!(restored.sample.path(), "/tmp/kick.wav");
        assert_eq!(restored.scale.snapshot().scl_name, "test.scl");
        assert_eq!(restored.ui_scale(), 1.75);
        assert!(
            shared.handoff.lock().incoming_sample.is_some(),
            "the restored sample is waiting for the audio thread"
        );
    }

    #[test]
    fn some_other_json_is_not_a_project() {
        let error = decode(br#"{"scl":"12 tones"}"#).unwrap_err();
        assert!(error.contains("not a readable project file"), "{error}");
    }

    #[test]
    fn a_file_from_a_newer_format_says_so_rather_than_half_loading() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode(state()).unwrap()).unwrap();
        value["format_version"] = serde_json::json!(FORMAT_VERSION + 1);
        let error = decode(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(error.contains("newer mater"), "{error}");
    }

    #[test]
    fn the_extension_is_added_only_when_it_is_missing() {
        assert_eq!(with_extension(Path::new("/a/b")), Path::new("/a/b.mater"));
        assert_eq!(
            with_extension(Path::new("/a/b.mater")),
            Path::new("/a/b.mater")
        );
        assert_eq!(
            with_extension(Path::new("/a/b.MATER")),
            Path::new("/a/b.MATER"),
            "a dialog that already appended it should not get a second one"
        );
        assert_eq!(
            with_extension(Path::new("/a/take.3")),
            Path::new("/a/take.3.mater"),
            "appended, so a typed name is never turned into a different file"
        );
    }

    #[test]
    fn the_offered_name_comes_from_the_sample() {
        assert_eq!(default_file_name("kick.wav"), "kick.mater");
        assert_eq!(default_file_name(""), "untitled.mater");
    }
}
