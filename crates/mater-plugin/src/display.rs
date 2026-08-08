//! What the display is scaled by, asked of the desktop rather than of the host.
//!
//! A host is supposed to tell a plugin how much the screen is scaled — CLAP's `gui.set_scale`, and
//! `Editor::set_scale_factor` at this end of it. Not every host does. Bitwig on Linux does not, and
//! a plugin that only listens for it then draws a 100 % interface on a 200 % desktop, at half the
//! size of everything around it.
//!
//! nih-plug's egui integration looks like it covers this: with no factor from the host it opens the
//! window under `WindowScalePolicy::SystemScaleFactor`, and baseview's X11 backend really does read
//! `Xft.dpi` and divide by 96. But the number stops there. Measured on a desktop with `Xft.dpi: 192`
//! and the host silent, `egui::Context::pixels_per_point` is 1.0 — the factor baseview worked out
//! never reaches the context that draws. So a point is a pixel, nothing anywhere applies the
//! desktop's scaling, and the interface has to apply it itself.
//!
//! Hence this: the same question baseview asks, asked again where the answer can be used. It is only
//! ever a *default* — see `editor::ui_scale`. A host that does report is believed over this, and a
//! scale set by hand is believed over both.

use std::sync::OnceLock;

/// The X resource `Xft.dpi` is in dots per inch against this, the unscaled baseline.
const BASELINE_DPI: f32 = 96.0;

/// How much the desktop scales itself, if that can be found out at all.
///
/// `None` where there is nobody to ask: no X display, a platform this does not cover, or a desktop
/// that has not set a DPI. The caller then has nothing better than 100 %.
///
/// Read once and remembered. This is a default for a freshly opened editor, not a live setting, and
/// a plugin has no business opening an X connection on every frame to re-ask.
pub fn system_scale() -> Option<f32> {
    static SCALE: OnceLock<Option<f32>> = OnceLock::new();
    *SCALE.get_or_init(detect)
}

/// The scaling a DPI reading means, or `None` if it is not a figure anything could be drawn at.
fn scale_from_dpi(dpi: f32) -> Option<f32> {
    // A zero would divide the layout into nothing, and a negative one is not a reading at all.
    (dpi.is_finite() && dpi > 0.0).then(|| dpi / BASELINE_DPI)
}

/// Pick `Xft.dpi` out of the X resource database's contents.
///
/// The format is one `key:\tvalue` per line, and the key we want may or may not be there. Kept
/// separate from the connection so the parsing can be tested without an X server.
fn xft_dpi(resources: &str) -> Option<f32> {
    resources.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "Xft.dpi").then(|| value.trim().parse().ok())?
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn detect() -> Option<f32> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let reply = connection
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            // In four-byte units, so a quarter of a megabyte of resources. Far more than the few
            // lines this holds, and the reply is truncated rather than refused if it somehow is not.
            64 * 1024,
        )
        .ok()?
        .reply()
        .ok()?;

    // Latin-1 by the letter of the protocol and ASCII in practice; a stray byte should cost us the
    // character rather than the reading.
    let resources = String::from_utf8_lossy(&reply.value);
    scale_from_dpi(xft_dpi(&resources)?)
}

/// Everywhere else. macOS scales windows itself and nih-plug declines the host's factor there
/// outright; Windows would want `GetDpiForSystem`, which is not wired up.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn detect() -> Option<f32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dpi_resource_is_picked_out_of_the_database() {
        let resources = "Xft.antialias:\t1\nXft.dpi:\t192\nXft.hinting:\t1\n";

        assert_eq!(xft_dpi(resources), Some(192.0));
    }

    #[test]
    fn a_database_without_one_says_so_rather_than_guessing() {
        assert_eq!(xft_dpi("Xft.antialias:\t1\nXcursor.size:\t24\n"), None);
        assert_eq!(xft_dpi(""), None);
    }

    #[test]
    fn a_key_that_merely_ends_in_the_one_we_want_is_not_it() {
        // `Xft.dpi` and `Foo.Xft.dpi` are different resources, and a suffix match would take either.
        assert_eq!(xft_dpi("Foo.Xft.dpi:\t384\n"), None);
        assert_eq!(xft_dpi("Xft.dpimode:\t384\n"), None);
    }

    #[test]
    fn a_reading_that_is_not_a_number_is_not_a_scaling() {
        assert_eq!(xft_dpi("Xft.dpi:\tlarge\n"), None);
        assert_eq!(xft_dpi("Xft.dpi\t192\n"), None);
    }

    #[test]
    fn the_dpi_is_read_against_the_unscaled_baseline() {
        assert_eq!(scale_from_dpi(96.0), Some(1.0));
        assert_eq!(scale_from_dpi(120.0), Some(1.25));
        assert_eq!(scale_from_dpi(192.0), Some(2.0));
    }

    #[test]
    fn a_dpi_nothing_could_be_drawn_at_is_refused() {
        // Rather than handing back a scaling of zero, which would divide the layout away.
        assert_eq!(scale_from_dpi(0.0), None);
        assert_eq!(scale_from_dpi(-96.0), None);
        assert_eq!(scale_from_dpi(f32::NAN), None);
    }
}
