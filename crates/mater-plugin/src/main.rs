//! Standalone build, for playing Mater without a host and for driving the editor during
//! development. The CLAP plugin is the real deliverable; this is a convenience wrapper.

use mater::Mater;
use nih_plug::prelude::*;

fn main() {
    nih_export_standalone::<Mater>();
}
