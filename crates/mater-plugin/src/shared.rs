//! State shared between the audio thread, the editor and background loading.
//!
//! The audio thread never blocks and never allocates: it `try_lock`s the handoff once per block,
//! swaps in anything waiting, and parks whatever it displaced back in the same slot so the main
//! thread does the deallocating.

use granny_core::sample::SampleBuffer;
use granny_core::scala::ScalaTuning;
use nih_plug::prelude::AtomicF32;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Maximum number of simultaneous voices.
pub const MAX_VOICES: usize = 16;

/// Playhead value meaning "this voice is not sounding".
pub const PLAYHEAD_IDLE: f32 = -1.0;

/// Values in flight between the main thread and the audio thread, in both directions.
#[derive(Default)]
pub struct Handoff {
    pub incoming_sample: Option<Arc<SampleBuffer>>,
    pub incoming_scala: Option<Option<Arc<ScalaTuning>>>,
    /// Displaced values, waiting for the main thread to drop them.
    pub retired: Vec<Arc<SampleBuffer>>,
    pub retired_scala: Vec<Arc<ScalaTuning>>,
}

pub struct Shared {
    pub handoff: Mutex<Handoff>,
    /// The sample the editor draws. Only touched by the main thread.
    pub editor_sample: Mutex<Arc<SampleBuffer>>,
    /// Bumped on every successful load so the editor can invalidate its cached waveform mesh.
    pub sample_generation: AtomicUsize,
    /// Per-voice playhead as a fraction of the sample, or [`PLAYHEAD_IDLE`].
    pub playheads: Vec<AtomicF32>,
    /// Last message to show in the editor: what loaded, or why it did not.
    pub status: Mutex<String>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            handoff: Mutex::new(Handoff::default()),
            editor_sample: Mutex::new(Arc::new(SampleBuffer::default())),
            sample_generation: AtomicUsize::new(0),
            playheads: (0..MAX_VOICES)
                .map(|_| AtomicF32::new(PLAYHEAD_IDLE))
                .collect(),
            status: Mutex::new("no sample loaded".into()),
        }
    }
}

impl Shared {
    /// Hand a freshly decoded sample to the audio thread and to the editor. Main thread only.
    pub fn publish_sample(&self, sample: Arc<SampleBuffer>) {
        *self.editor_sample.lock() = sample.clone();
        self.handoff.lock().incoming_sample = Some(sample);
        self.sample_generation.fetch_add(1, Ordering::Release);
    }

    /// Hand a new (or cleared) Scala tuning to the audio thread. Main thread only.
    pub fn publish_scala(&self, scala: Option<Arc<ScalaTuning>>) {
        self.handoff.lock().incoming_scala = Some(scala);
    }

    pub fn set_status(&self, message: impl Into<String>) {
        *self.status.lock() = message.into();
    }

    pub fn status(&self) -> String {
        self.status.lock().clone()
    }

    /// Drop anything the audio thread displaced. Main thread only.
    pub fn collect_garbage(&self) {
        let (samples, scales) = {
            let mut handoff = self.handoff.lock();
            (
                std::mem::take(&mut handoff.retired),
                std::mem::take(&mut handoff.retired_scala),
            )
        };
        // Dropped here, outside the lock.
        drop(samples);
        drop(scales);
    }
}
