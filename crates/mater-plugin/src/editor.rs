//! The editor: a waveform you can work on directly, with the parameters underneath.
//!
//! The hardware's interface is four knobs, a two-page toggle and a four-digit display. That makes
//! sense when you are holding it. On screen, with sixteen independent voices crawling over the
//! sample at once, the waveform is the interface: start and end are handles you drag, the grain
//! window is shaded, and every sounding voice draws its own playhead.

use granny_core::curves::{grain_ms, shift_bytes};
use granny_core::pitch::describe_note;
use granny_core::sample::SampleBuffer;
use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor, egui, resizable_window::paint_resize_corner, widgets, EguiState,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::display;
use crate::params::{describe_sample, MaterParams, DEFAULT_WINDOW, UI_SCALES};
use crate::project;
use crate::shared::{Shared, PLAYHEAD_IDLE};
use crate::{Mater, Task};

/// The one accent colour: handles, playheads, filled sliders, anything switched on.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(255, 146, 38);
/// The same hue lifted, for hover and for text that has to stay legible on the accent.
const ACCENT_BRIGHT: egui::Color32 = egui::Color32::from_rgb(255, 187, 112);
/// And dropped, for fills that sit behind text — slider bars, selected ranges.
const ACCENT_FILL: egui::Color32 = egui::Color32::from_rgb(122, 60, 10);

/// Normal text, against the near-black everything here is painted on.
///
/// egui's dark theme puts this at gray(140), which is about 5:1 — a quarter of the contrast a host
/// carries on its own chrome, and low enough that a parameter name reads as faint at any size. It
/// was the reason this interface seemed to want scaling up: the names were not too small so much as
/// too dim, and making them larger was the only lever that touched them. This is about 10:1.
///
/// Section headings escaped it by being `strong()`, which is white whatever this says — which is
/// why they looked right while the labels beneath them did not.
const TEXT: egui::Color32 = egui::Color32::from_gray(200);
/// What dimmed text is tinted halfway towards.
///
/// egui fades a disabled `Ui` — and anything `weak()` — halfway to
/// [`egui::Visuals::fade_out_to_color`], whose only source is `noninteractive.weak_bg_fill`. At the
/// gray(27) it ships as, that target *is* the background, so a dimmed parameter landed at 2.2:1:
/// greyed past reading rather than out of the way. Lifting the target leaves dimmed text at about
/// the contrast normal text used to have, which is the right end of "clearly secondary".
const TEXT_FADE: egui::Color32 = egui::Color32::from_gray(75);

/// The share of the window the waveform takes when the controls want everything else, and the
/// bounds that share is held within. Given a window with room to spare it takes more — see
/// [`wave_height`].
const WAVE_FRACTION: f32 = 0.4;
const WAVE_MIN_HEIGHT: f32 = 160.0;
const WAVE_MAX_HEIGHT: f32 = 360.0;
/// The gap above and below the waveform.
const SECTION_GAP: f32 = 6.0;
/// How close to a handle the pointer must be to grab it.
const HANDLE_GRAB_PX: f32 = 10.0;
/// Width of one column of the parameter grid. Every cell is a whole number of these, so however
/// the rows wrap they still line up.
///
/// Wide enough to hold the longest caption line whole at the size [`text_styles`] draws it. The
/// worst of them is grain size, whose name and `100 (1666 ms, 288 ticks)` came to about 162 of the
/// 190 this used to be while the reading was set in 9 points, and to within a couple of points of
/// the whole column at 10.5. The reading truncates rather than overflowing, so what that width
/// bought was a tooltip where the number used to be.
const CELL_WIDTH: f32 = 215.0;
const CELL_HEIGHT: f32 = 42.0;
/// Height of the control inside a cell, and of the line above it that names the parameter and
/// reads its value. That line is a fixed height because clicking a reading to type an exact value
/// swaps it for a text field, and a taller field would jog every row below it.
const CONTROL_HEIGHT: f32 = 20.0;
const HEADER_HEIGHT: f32 = 18.0;
/// Room between the interface and the window's edges. The panel's own margin is no use: the
/// resizable window hands its contents the full clip rect, which is outside that margin.
const WINDOW_PADDING: f32 = 8.0;

/// Every size the editor lays out by hand, multiplied by the current ui scale.
///
/// egui's own zoom factor is no use here: this integration hands egui a screen rect derived from
/// the native scale alone, so changing the zoom would leave layout and rendering disagreeing. The
/// style and these metrics carry the scaling instead, and a point stays a pixel.
#[derive(Copy, Clone)]
struct Metrics {
    scale: f32,
}

impl Metrics {
    /// A parameter cell that many columns of the grid wide.
    fn cell(self, ui: &egui::Ui, columns: usize) -> egui::Vec2 {
        egui::vec2(self.span(ui, columns), CELL_HEIGHT * self.scale)
    }

    /// How wide that many columns are, the gaps between them included.
    fn span(self, ui: &egui::Ui, columns: usize) -> f32 {
        let columns = columns.max(1) as f32;
        CELL_WIDTH * self.scale * columns + ui.spacing().item_spacing.x * (columns - 1.0)
    }

    /// How many whole columns it takes to hold something this wide.
    fn columns_for(self, ui: &egui::Ui, width: f32) -> usize {
        // A hair of slack either way, so that something exactly a whole number of columns wide is
        // neither pushed into one more nor cut down to one fewer by the last bit of arithmetic.
        (self.columns(ui, width) - 0.001).ceil().max(1.0) as usize
    }

    /// And how many fit within it.
    fn columns_in(self, ui: &egui::Ui, width: f32) -> usize {
        (self.columns(ui, width) + 0.001).floor().max(1.0) as usize
    }

    fn columns(self, ui: &egui::Ui, width: f32) -> f32 {
        let gap = ui.spacing().item_spacing.x;
        (width + gap) / (CELL_WIDTH * self.scale + gap)
    }

    /// The control that sits inside a one-column cell.
    fn control(self) -> egui::Vec2 {
        egui::vec2(CELL_WIDTH * self.scale, CONTROL_HEIGHT * self.scale)
    }

    /// The gap held between the interface and the window's edges.
    fn padding(self) -> egui::Margin {
        egui::Margin::same(self.at(WINDOW_PADDING) as i8)
    }

    fn at(self, points: f32) -> f32 {
        points * self.scale
    }
}

/// Which handle the pointer is currently dragging.
#[derive(Copy, Clone, PartialEq, Eq, Default)]
enum Drag {
    #[default]
    None,
    Start,
    End,
}

#[derive(Default)]
struct EditorState {
    drag: Drag,
    /// The scale the style was last built for, so it is rebuilt only when it changes.
    styled_for: Option<f32>,
    /// The height everything under the waveform wanted when it was last drawn, which is what the
    /// waveform gives up to it. Nothing is known about it until it has been drawn once.
    controls_height: Option<f32>,
}

/// Takes the host's DPI scaling, and remembers what it is so the layout can divide it back out.
///
/// The factor is real and worth having: it is what makes the window the right size for the screen
/// and the text sharp on it. What it must not do is change how large the interface *looks*. It used
/// to, by a route nobody could see: nih-plug's egui integration refuses a scale factor while the
/// editor is open and applies it the *next* time the window opens, so reopening the editor under a
/// host set to 200 % doubled everything while `ui scale` still read 100 %. See [`layout_scale`].
struct HostDpi {
    inner: Box<dyn Editor>,
    shared: Arc<Shared>,
}

impl Editor for HostDpi {
    fn set_scale_factor(&self, factor: f32) -> bool {
        // Only remember what was actually taken: the integration turns a factor down while the
        // window is open, and the wrapper then keeps sizing everything by the previous one.
        let accepted = self.inner.set_scale_factor(factor);
        if accepted {
            self.shared.host_dpi.store(factor, Ordering::Relaxed);
            self.shared.host_dpi_reported.store(true, Ordering::Relaxed);
        }
        accepted
    }

    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        self.inner.spawn(parent, context)
    }

    fn size(&self) -> (u32, u32) {
        self.inner.size()
    }

    fn param_value_changed(&self, id: &str, normalized_value: f32) {
        self.inner.param_value_changed(id, normalized_value);
    }

    fn param_modulation_changed(&self, id: &str, modulation_offset: f32) {
        self.inner.param_modulation_changed(id, modulation_offset);
    }

    fn param_values_changed(&self) {
        self.inner.param_values_changed();
    }
}

pub fn create(
    params: Arc<MaterParams>,
    shared: Arc<Shared>,
    async_executor: AsyncExecutor<Mater>,
) -> Option<Box<dyn Editor>> {
    let egui_state = params.editor_state.clone();
    let host_scale = shared.clone();

    let inner = create_egui_editor(
        egui_state.clone(),
        EditorState::default(),
        // A reopened window is a fresh context with egui's own style, so ask for ours again.
        |_, state: &mut EditorState| state.styled_for = None,
        move |ctx, setter, state| {
            // Anything the audio thread displaced is dropped here, on the main thread.
            shared.collect_garbage();
            handle_dropped_files(ctx, setter, &shared, &async_executor);

            let host = HostScale::read(&shared);
            let scale = layout_scale(ui_scale(&params, host, display_scale()), host.factor);
            let opening = state.styled_for.is_none();
            if state.styled_for != Some(scale) {
                apply_style(ctx, scale);
                state.styled_for = Some(scale);
            }
            let metrics = Metrics { scale };
            if opening {
                size_for_scale(ctx, &egui_state, setter, scale);
            }

            window(ctx, &egui_state, setter, egui::vec2(640.0, 480.0), |ui| {
                egui::Frame::NONE
                    .inner_margin(metrics.padding())
                    .show(ui, |ui| {
                        let sample = shared.editor_sample.lock().clone();

                        header(
                            ui,
                            &params,
                            &shared,
                            &egui_state,
                            setter,
                            &async_executor,
                            &sample,
                        );
                        ui.add_space(metrics.at(SECTION_GAP));
                        waveform(ui, &params, &shared, setter, state, &sample, metrics);
                        ui.add_space(metrics.at(SECTION_GAP));

                        let controls = egui::ScrollArea::vertical().show(ui, |ui| {
                            // The bar is drawn over the right edge of the pane rather than beside
                            // it, so keep the grid clear of it: a cell that takes the last of the
                            // width would otherwise be laid out underneath the bar.
                            ui.set_max_width(
                                (ui.available_width() - ui.spacing().scroll.bar_width).max(1.0),
                            );
                            knobs(ui, &params, setter, metrics);
                            ui.separator();
                            settings(ui, &params, setter, metrics);
                            ui.separator();
                            tuning(ui, &params, setter, &async_executor, &sample, metrics);
                            ui.separator();
                            mod_matrix(ui, &params, setter, metrics);
                            ui.separator();
                            fidelity(ui, &params, setter, metrics);
                        });
                        // What the controls wanted, which is what the waveform leaves them next
                        // time round. They are the same height whatever the waveform does — they
                        // wrap on the window's width — so this settles rather than oscillating.
                        state.controls_height = Some(controls.content_size.y);
                    });
            });
        },
    )?;

    Some(Box::new(HostDpi {
        inner,
        shared: host_scale,
    }))
}

/// The window, with a corner to drag it larger by.
///
/// This is nih-plug's own `ResizableWindow` with the units put right. That one asks the host for a
/// size in points and then resizes the drawing surface to that many *pixels* — the same number only
/// while the host's DPI scaling is 1. At 200 % the surface comes out half the window, and since
/// OpenGL counts from the bottom left, the interface ends up painted into the bottom-left quarter
/// with the rest of the window left black. Resizing by the route the window's initial size already
/// takes keeps the two in step at any scaling.
fn window(
    ctx: &egui::Context,
    state: &Arc<EguiState>,
    setter: &ParamSetter,
    min_size: egui::Vec2,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        let rect = ui.clip_rect();
        let mut content = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
        add_contents(&mut content);

        let corner_size = egui::Vec2::splat(ui.visuals().resize_corner_size);
        let corner_rect = egui::Rect::from_min_size(rect.max - corner_size, corner_size);
        let corner = ui.interact(
            corner_rect,
            egui::Id::new("resize corner"),
            egui::Sense::drag(),
        );

        if let Some(pointer) = corner.interact_pointer_pos() {
            if corner.dragged() {
                resize(
                    ctx,
                    state,
                    setter,
                    (pointer - rect.min + 0.5 * corner.rect.size()).max(min_size),
                );
            }
        }

        paint_resize_corner(&content, &corner);
    });
}

/// Ask for a window this many points across.
///
/// Points are exactly what the host means by logical pixels here, because the factor it scales
/// those by is the one egui is drawing at. So the same number goes to both parties that have to
/// agree: the host, which owns the window and reads the size back off the editor, and the window
/// itself, which has to follow it.
fn resize(ctx: &egui::Context, state: &Arc<EguiState>, setter: &ParamSetter, size: egui::Vec2) {
    let asked = (
        size.x.round().max(1.0) as u32,
        size.y.round().max(1.0) as u32,
    );
    if state.size() == asked {
        return;
    }

    // `EguiState` keeps its size to itself. The persistence trait, which is how a host restoring a
    // project sets it, is the way in.
    if let Ok(resized) = Arc::try_unwrap(EguiState::from_size(asked.0, asked.1)) {
        PersistentField::set(state, resized);
    }
    setter.raw_context.request_resize();
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
}

/// What the host has said about scaling, if it has said anything.
///
/// Both halves are needed: [`Self::factor`] starts at 1.0, which is also what a host scaling by
/// 100 % would send, so the number alone cannot tell a host that means it from one that has never
/// spoken. Those two want different answers — believe the first, go looking past the second.
#[derive(Copy, Clone)]
struct HostScale {
    factor: f32,
    reported: bool,
}

impl HostScale {
    fn read(shared: &Shared) -> Self {
        Self {
            factor: shared.host_dpi.load(Ordering::Relaxed),
            reported: shared.host_dpi_reported.load(Ordering::Relaxed),
        }
    }
}

/// How large the interface should draw itself, in the units the `ui scale` readout is in.
///
/// Three sources, in order of how much they know. A scale set by hand is exactly itself, on any
/// host, for good. Failing that the host's own factor, because a host scaling its interface by 200 %
/// has sized this window for a 200 % interface and drawing a 100 % one inside it would leave the
/// rest of the window empty. Failing *that* — a host that never reports one, which is not rare —
/// the desktop's own scaling, which is the same thing the host would have been passing on.
///
/// `display` is taken as an argument rather than read here so the choosing can be tested without an
/// X server on the other end of it. See [`display_scale`].
fn ui_scale(params: &MaterParams, host: HostScale, display: f32) -> f32 {
    if params.ui_scale_is_set() {
        params.ui_scale()
    } else if host.reported && host.factor > 0.0 {
        host.factor
    } else {
        display
    }
}

/// What the desktop says it is scaled by, held to the range the steps offer.
///
/// Clamped where the host's factor is not, because this one we went looking for: a desktop with an
/// odd `Xft.dpi` should give an interface at the nearest size that makes sense rather than one drawn
/// at six times the size or a sixth of it.
fn display_scale() -> f32 {
    display::system_scale()
        .map(|scale| scale.clamp(UI_SCALES[0], UI_SCALES[UI_SCALES.len() - 1]))
        .unwrap_or(UI_SCALES[0])
}

/// What one laid-out point is worth, with the host's DPI scaling divided back out of `ui scale`.
///
/// The host's factor multiplies every point on its way to the screen, so leaving it in would make
/// the two scales compound: a host at 200 % would draw a 100 % interface at double size, and the
/// size would change whenever the host announced a new factor. Dividing it out means `ui scale` is
/// a size on screen and nothing else moves it, while the host's factor still does what it is for —
/// a window sized for the display, drawn at its full resolution.
///
/// Only the *host's* factor, and deliberately: where the host reports nothing this is 1.0 and
/// nothing is divided out, which is right, because then nothing is multiplying the points either.
/// A scaling found by asking the desktop ourselves has to be drawn rather than divided — see
/// [`display`].
fn layout_scale(ui_scale: f32, host_dpi: f32) -> f32 {
    if host_dpi > 0.0 {
        ui_scale / host_dpi
    } else {
        ui_scale
    }
}

/// Rebuild the style for a given scale.
///
/// Everything here is set from a constant rather than adjusted from what is already there, so
/// applying it repeatedly cannot compound.
fn apply_style(ctx: &egui::Context, scale: f32) {
    ctx.all_styles_mut(|style| {
        style.text_styles = text_styles(scale);
        style.spacing = spacing(scale);
        paint(&mut style.visuals, scale);
    });
}

/// egui's default text styles, a little larger, scaled.
///
/// `Small` carries most of the interface rather than the captions it is named for — every parameter
/// name and every value reading is set in it — and egui's 9 points for that is a caption size, small
/// enough on a laptop panel to be hard work. The sizes here are a step up from the defaults so that
/// the text is readable at a scale whose *window* still fits such a screen: `ui scale` grows the
/// layout and the window with it, so turning it up is no answer when there is no room to grow into.
///
/// The layout is not scaled to match — a cell is the same height, and the same number of columns
/// fit the window it opens at — so this is text taking up more of the room already set aside for it.
/// [`CELL_WIDTH`] is the one size that had to give, being the width the readings are measured
/// against.
fn text_styles(scale: f32) -> BTreeMap<egui::TextStyle, egui::FontId> {
    use egui::FontFamily::{Monospace, Proportional};
    use egui::{FontId, TextStyle};

    [
        (TextStyle::Small, FontId::new(10.5 * scale, Proportional)),
        (TextStyle::Body, FontId::new(13.5 * scale, Proportional)),
        (TextStyle::Button, FontId::new(13.5 * scale, Proportional)),
        (TextStyle::Heading, FontId::new(18.0 * scale, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0 * scale, Monospace)),
    ]
    .into()
}

/// egui's default spacing, scaled. The fields left alone are widths of things we do not use.
fn spacing(scale: f32) -> egui::style::Spacing {
    let base = egui::style::Spacing::default();
    let margin = |margin: egui::Margin| egui::Margin::same((f32::from(margin.left) * scale) as i8);

    egui::style::Spacing {
        item_spacing: base.item_spacing * scale,
        window_margin: margin(base.window_margin),
        menu_margin: margin(base.menu_margin),
        button_padding: base.button_padding * scale,
        indent: base.indent * scale,
        interact_size: base.interact_size * scale,
        slider_width: base.slider_width * scale,
        slider_rail_height: base.slider_rail_height * scale,
        combo_width: base.combo_width * scale,
        text_edit_width: base.text_edit_width * scale,
        icon_width: base.icon_width * scale,
        icon_width_inner: base.icon_width_inner * scale,
        icon_spacing: base.icon_spacing * scale,
        scroll: egui::style::ScrollStyle {
            bar_width: base.scroll.bar_width * scale,
            handle_min_length: base.scroll.handle_min_length * scale,
            ..base.scroll
        },
        ..base
    }
}

/// Repaint egui's blues — selection, links, the text cursor — in the accent orange.
fn paint(visuals: &mut egui::Visuals, scale: f32) {
    visuals.selection.bg_fill = ACCENT_FILL;
    visuals.selection.stroke = egui::Stroke::new(scale, ACCENT_BRIGHT);
    visuals.hyperlink_color = ACCENT;
    visuals.text_cursor.stroke = egui::Stroke::new(2.0 * scale, ACCENT);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(scale, ACCENT);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(scale, ACCENT_BRIGHT);
    visuals.resize_corner_size = 12.0 * scale;
    // Text last, and both of these rather than one: lifting normal text without lifting what dimmed
    // text fades towards would only widen the gap between them. See [`TEXT`] and [`TEXT_FADE`].
    visuals.widgets.noninteractive.fg_stroke.color = TEXT;
    visuals.widgets.noninteractive.weak_bg_fill = TEXT_FADE;
}

fn handle_dropped_files(
    ctx: &egui::Context,
    setter: &ParamSetter,
    shared: &Arc<Shared>,
    executor: &AsyncExecutor<Mater>,
) {
    let dropped = ctx.input(|input| input.raw.dropped_files.clone());
    for file in dropped {
        let Some(path) = file.path else { continue };
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("scl") => executor.execute_background(Task::LoadScl(path)),
            Some("kbm") => executor.execute_background(Task::LoadKbm(path)),
            Some(project::EXTENSION) => load_project(&path, setter, shared),
            _ => executor.execute_background(Task::LoadSample(path)),
        }
    }
}

/// Write everything the plugin persists — the sample, the tuning, every parameter — to one file.
fn save_project(path: &Path, setter: &ParamSetter, shared: &Arc<Shared>) {
    let state = setter.raw_context.get_state();
    match project::save(path, state) {
        Ok(written) => shared.set_status(format!("saved project {}", file_label(&written))),
        Err(err) => shared.set_status(format!("could not save project: {err}")),
    }
}

/// Read one back and hand it to the wrapper, which restores every parameter and every persisted
/// field and reinitialises the plugin — the same path a host takes when it reopens a project.
fn load_project(path: &Path, setter: &ParamSetter, shared: &Arc<Shared>) {
    match project::load(path) {
        Ok(state) => {
            let label = file_label(path);
            // Restoring the sample writes a status of its own, so ours has to land after it.
            setter.raw_context.set_state(state);
            shared.set_status(format!("loaded project {label}"));
        }
        Err(err) => shared.set_status(format!("could not load project: {err}")),
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string()
}

fn project_dialog() -> rfd::FileDialog {
    rfd::FileDialog::new().add_filter("mater project", &[project::EXTENSION])
}

fn header(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    shared: &Arc<Shared>,
    state: &Arc<EguiState>,
    setter: &ParamSetter,
    executor: &AsyncExecutor<Mater>,
    sample: &SampleBuffer,
) {
    ui.horizontal(|ui| {
        ui.heading("mater");
        ui.separator();

        if ui.button("save project…").clicked() {
            if let Some(path) = project_dialog()
                .set_file_name(project::default_file_name(&sample.name))
                .save_file()
            {
                save_project(&path, setter, shared);
            }
        }
        if ui.button("load project…").clicked() {
            if let Some(path) = project_dialog().pick_file() {
                load_project(&path, setter, shared);
            }
        }
        ui.separator();

        if ui.button("load sample…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter(
                    "audio",
                    &["wav", "aiff", "aif", "flac", "mp3", "ogg", "m4a", "caf"],
                )
                .pick_file()
            {
                executor.execute_background(Task::LoadSample(path));
            }
        }

        let path = params.sample.path();
        if !path.is_empty() && ui.button("reload").clicked() {
            executor.execute_background(Task::LoadSample(path.into()));
        }

        ui.label(describe_sample(sample));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            scale_control(ui, params, shared, state, setter);
        });
    });

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(shared.status()).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("drop an audio, .mater, .scl or .kbm file anywhere").weak(),
            );
        });
    });
}

/// How large the interface draws itself, in steps. Laid out right to left, so it reads
/// `ui scale [−] 125 % [+]`.
fn scale_control(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    shared: &Arc<Shared>,
    state: &Arc<EguiState>,
    setter: &ParamSetter,
) {
    // The size being drawn, not what the layout is working in: the host's DPI scaling has been
    // divided out of that one, and reporting it would show 100 % on a host set to 200 %.
    let host = HostScale::read(shared);
    let scale = ui_scale(params, host, display_scale());
    // The nearest step, so a state saved by a future version with other steps still lands somewhere.
    let step = UI_SCALES
        .iter()
        .position(|&candidate| candidate >= scale)
        .unwrap_or(UI_SCALES.len() - 1);

    let larger = ui.add_enabled(step + 1 < UI_SCALES.len(), egui::Button::new("+"));
    if larger.clicked() {
        rescale(ui.ctx(), params, state, setter, scale, UI_SCALES[step + 1]);
    }

    // This is the only thing that sets the size of the interface — see `layout_scale` — so say
    // where the number came from. Both halves, always: the host's factor used to be named only
    // while the scale was still following it, so the moment you set the scale by hand to work
    // around the interface coming out wrong, the one number that says whether the host is scaling
    // at all went out of reach. That is precisely when it is worth reading.
    ui.label(egui::RichText::new(format!("{:.0} %", scale * 100.0)).monospace())
        .on_hover_text(format!(
            "{}\n{}",
            if params.ui_scale_is_set() {
                "how large the interface draws itself, saved with the instance"
            } else {
                "how large the interface draws itself — following the scaling below until you \
                 set it here, after which it is yours"
            },
            match (host.reported, display::system_scale()) {
                (true, _) => format!("the host reports {:.0} % scaling", host.factor * 100.0),
                (false, Some(display)) => format!(
                    "the host reports no scaling of its own; the desktop's is {:.0} %",
                    display * 100.0
                ),
                (false, None) => {
                    "neither the host nor the desktop reports any scaling".to_string()
                }
            }
        ));

    let smaller = ui.add_enabled(step > 0, egui::Button::new("−"));
    if smaller.clicked() {
        rescale(ui.ctx(), params, state, setter, scale, UI_SCALES[step - 1]);
    }

    ui.label(egui::RichText::new("ui scale").weak());
}

/// Draw the interface at a new size, and ask for a window that fits it.
///
/// Without the second half, making the interface smaller would just leave the bottom of the window
/// empty, and making it larger would push everything out of sight until the corner was dragged.
fn rescale(
    ctx: &egui::Context,
    params: &Arc<MaterParams>,
    state: &Arc<EguiState>,
    setter: &ParamSetter,
    from: f32,
    to: f32,
) {
    params.set_ui_scale(to);
    resize(ctx, state, setter, rescaled(state.size(), from, to));
}

/// The window that holds an interface redrawn from one scale at another: the same window, by the
/// ratio between them. A scale of zero is not one the steps offer, but dividing by it would hand
/// back a window of nan.
fn rescaled(size: (u32, u32), from: f32, to: f32) -> egui::Vec2 {
    let (width, height) = (size.0 as f32, size.1 as f32);
    if from <= 0.0 {
        return egui::vec2(width, height);
    }
    let ratio = to / from;
    egui::vec2(width * ratio, height * ratio)
}

/// Size a window nobody has chosen yet for the scale it is about to be drawn at.
///
/// Changing scale by the steps resizes the window with it — see [`rescale`] — because an interface
/// drawn larger in the same window is one that no longer fits. A scale that arrives *without* a
/// click gets no such resize, and a fresh instance under a host that reports nothing takes its scale
/// from the desktop (see [`display`]). At a 200 % desktop that drew a 200 % interface into the
/// 960 x 700 laid out for a 100 % one: header controls printed over each other, the grid down to two
/// columns, everything below the knobs pushed out of sight.
///
/// Only a window still at exactly [`DEFAULT_WINDOW`] is touched — any other size was dragged there
/// or restored from a project, and is theirs to keep. And only on opening, so that dragging a window
/// down to that size is not undone under the pointer.
fn size_for_scale(ctx: &egui::Context, state: &Arc<EguiState>, setter: &ParamSetter, scale: f32) {
    if state.size() == DEFAULT_WINDOW {
        resize(ctx, state, setter, rescaled(DEFAULT_WINDOW, 1.0, scale));
    }
}

/// How tall the waveform draws, given the height left under the header and the height the controls
/// wanted when they were last drawn.
///
/// Whatever those controls do not need, the waveform takes: a window with room to spare shows more
/// of the sample rather than a band of nothing along the bottom, at any scale and whatever size a
/// host or a window manager decides the window is. Where there is nothing spare — the window the
/// editor opens at is already smaller than everything wants, and the controls scroll — it falls
/// back to its share of what there is, which is what it has always taken.
fn wave_height(available: f32, controls: Option<f32>, metrics: Metrics) -> f32 {
    let share = (available * WAVE_FRACTION).clamp(
        metrics.at(WAVE_MIN_HEIGHT),
        metrics.at(WAVE_MAX_HEIGHT).max(metrics.at(WAVE_MIN_HEIGHT)),
    );
    match controls {
        // Nothing drawn yet, so nothing is known about what the rest of it wants.
        None => share,
        Some(controls) => (available - controls - metrics.at(SECTION_GAP)).max(share),
    }
}

/// Draw the waveform, its handles, the grain window and every live playhead.
fn waveform(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    shared: &Arc<Shared>,
    setter: &ParamSetter,
    state: &mut EditorState,
    sample: &SampleBuffer,
    metrics: Metrics,
) {
    let height = wave_height(ui.available_height(), state.controls_height, metrics);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();
    // The painter is given fonts by hand, so take them from the same styles the widgets below
    // resolve rather than naming sizes here — those would be the only text in the editor that
    // [`text_styles`] did not move. Already scaled by the time they come back, so unlike every
    // other size in this function they do not go through [`Metrics::at`].
    let prompt_font = egui::TextStyle::Body.resolve(ui.style());
    let handle_font = egui::TextStyle::Monospace.resolve(ui.style());

    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);

    if sample.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no sample — click “load sample…” or drop a file here",
            prompt_font,
            visuals.weak_text_color(),
        );
        return;
    }

    let start_fraction = params.start.value() as f32 / 1023.0;
    let end_value = params.end.value();
    let end_fraction = if end_value >= 1000 {
        1.0
    } else {
        end_value as f32 / 1023.0
    };
    let x_at = |fraction: f32| rect.left() + fraction.clamp(0.0, 1.0) * rect.width();

    // The active loop region, so the excluded parts read as inactive.
    let loop_rect = egui::Rect::from_x_y_ranges(
        x_at(start_fraction.min(end_fraction))..=x_at(start_fraction.max(end_fraction)),
        rect.y_range(),
    );
    painter.rect_filled(loop_rect, 0.0, visuals.faint_bg_color);

    // Peaks.
    let peaks = sample.peaks();
    let mid_y = rect.center().y;
    let half = rect.height() * 0.5 - 4.0;
    let mut shapes = Vec::with_capacity(peaks.len());
    for (index, &(low, high)) in peaks.iter().enumerate() {
        let x = rect.left() + (index as f32 / peaks.len() as f32) * rect.width();
        // 8-bit unsigned, so 128 is the zero line.
        let top = mid_y - ((high as f32 - 128.0) / 128.0) * half;
        let bottom = mid_y - ((low as f32 - 128.0) / 128.0) * half;
        let inside = x >= loop_rect.left() && x <= loop_rect.right();
        let color = if inside {
            visuals.text_color()
        } else {
            visuals.weak_text_color()
        };
        shapes.push(egui::Shape::line_segment(
            [egui::pos2(x, top), egui::pos2(x, bottom.max(top + 1.0))],
            egui::Stroke::new(1.0, color),
        ));
    }
    painter.extend(shapes);

    // Grain window: how much of the sample one grain covers before the next shift.
    let grain = params.grain.value();
    if grain > 0 {
        let milliseconds = grain_ms(grain as u16, params.curve_mode()) as f32;
        let bytes = milliseconds * 22.05; // at the native 22050 Hz
        let width = (bytes / sample.len() as f32).clamp(0.0, 1.0) * rect.width();
        let grain_rect = egui::Rect::from_min_size(
            egui::pos2(loop_rect.left(), rect.top()),
            egui::vec2(width.max(2.0), rect.height()),
        );
        painter.rect_filled(grain_rect, 0.0, ACCENT.gamma_multiply(0.18));
    }

    // Playheads.
    for slot in &shared.playheads {
        let position = slot.load(Ordering::Relaxed);
        if position <= PLAYHEAD_IDLE {
            continue;
        }
        let x = x_at(position);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(metrics.at(1.5), ACCENT),
        );
    }

    // Handles.
    for (fraction, label) in [(start_fraction, "s"), (end_fraction, "e")] {
        let x = x_at(fraction);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(metrics.at(2.0), ACCENT_BRIGHT),
        );
        painter.text(
            egui::pos2(x + metrics.at(3.0), rect.top() + metrics.at(2.0)),
            egui::Align2::LEFT_TOP,
            label,
            handle_font.clone(),
            ACCENT_BRIGHT,
        );
    }

    // Dragging. Grab whichever handle is nearest when the drag begins, then follow the pointer.
    if response.drag_started() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let to_start = (pointer.x - x_at(start_fraction)).abs();
            let to_end = (pointer.x - x_at(end_fraction)).abs();
            state.drag = if to_start.min(to_end) > metrics.at(HANDLE_GRAB_PX) {
                Drag::None
            } else if to_start <= to_end {
                Drag::Start
            } else {
                Drag::End
            };
            match state.drag {
                Drag::Start => setter.begin_set_parameter(&params.start),
                Drag::End => setter.begin_set_parameter(&params.end),
                Drag::None => {}
            }
        }
    }

    if response.dragged() && state.drag != Drag::None {
        if let Some(pointer) = response.interact_pointer_pos() {
            let fraction = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let value = (fraction * 1023.0).round() as i32;
            match state.drag {
                Drag::Start => setter.set_parameter(&params.start, value.min(1023)),
                Drag::End => setter.set_parameter(&params.end, value),
                Drag::None => {}
            }
        }
    }

    if response.drag_stopped() {
        match state.drag {
            Drag::Start => setter.end_set_parameter(&params.start),
            Drag::End => setter.end_set_parameter(&params.end),
            Drag::None => {}
        }
        state.drag = Drag::None;
    }
}

/// One parameter as a cell of the grid: what it is and what it currently reads on one line, the
/// control itself on the next, in a cell a whole number of columns wide.
///
/// The reading used to sit beside the control rather than above it, which is where
/// [`widgets::ParamSlider`] puts it. That made every cell as wide as its own value text happened to
/// be — `0 (off)` next to `1022 (end of sample)` — so no two rows shared a column and nothing could
/// be read down the page. A caption line takes the text out of the row, and what is left is the
/// same shape for every parameter whatever it reads.
fn cell(
    ui: &mut egui::Ui,
    label: &str,
    columns: usize,
    metrics: Metrics,
    reading: impl FnOnce(&mut egui::Ui),
    control: impl FnOnce(&mut egui::Ui),
) {
    let size = metrics.cell(ui, columns);
    ui.allocate_ui(size, |ui| {
        ui.vertical(|ui| {
            line(ui, egui::vec2(size.x, metrics.at(HEADER_HEIGHT)), |ui| {
                ui.label(egui::RichText::new(label).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), reading);
            });
            line(ui, egui::vec2(size.x, metrics.at(CONTROL_HEIGHT)), control);
        });
    });
}

/// One line of a cell, held at the size it is given however little goes in it — a checkbox is
/// narrow, and the cell still has to hold its column's width.
fn line(ui: &mut egui::Ui, size: egui::Vec2, contents: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        size,
        egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
        |ui| {
            ui.set_min_size(size);
            contents(ui);
        },
    );
}

/// Grey a cell out where its parameter has nothing to do.
///
/// [`egui::Ui::add_enabled_ui`] on its own is not enough inside a wrapping row: the scope it opens
/// takes whatever is left of the line and lays the cell out within that, so a cell that reaches the
/// scope with too little room left runs off the edge instead of moving down. Wrapping the row by
/// hand first is what a bare cell gets for free.
fn dimmed(ui: &mut egui::Ui, enabled: bool, metrics: Metrics, add: impl FnOnce(&mut egui::Ui)) {
    // What is left of the line, which is not what `available_width` reports in a wrapping row: that
    // one answers with the width of a whole row, on the grounds that a wrap is always available.
    if ui.available_rect_before_wrap().width() < metrics.span(ui, 1) {
        ui.end_row();
    }
    ui.add_enabled_ui(enabled, add);
}

/// One parameter as a slider, with its value in the caption line above.
fn labelled<'a>(
    ui: &mut egui::Ui,
    label: &str,
    param: &'a impl Param,
    setter: &'a ParamSetter,
    metrics: Metrics,
) {
    let width = metrics.span(ui, 1);
    cell(
        ui,
        label,
        1,
        metrics,
        |ui| reading(ui, param, setter),
        |ui| {
            ui.add(
                widgets::ParamSlider::for_param(param, setter)
                    .without_value()
                    .with_width(width),
            );
        },
    );
}

/// A switch in the same cell shape as [`labelled`].
///
/// On and off are states, not values you slide between, so they get a checkbox — and the accent
/// colour, so a row of them can be read at a glance. The word goes in the caption line where every
/// other parameter's value is rather than beside the box, so that the columns read straight down.
fn toggle(
    ui: &mut egui::Ui,
    label: &str,
    param: &BoolParam,
    setter: &ParamSetter,
    metrics: Metrics,
) {
    let current = param.value();
    cell(
        ui,
        label,
        1,
        metrics,
        |ui| {
            let text = egui::RichText::new(if current { "on" } else { "off" }).small();
            ui.label(if current {
                text.color(ACCENT)
            } else {
                text.weak()
            });
        },
        |ui| {
            let mut value = current;
            if current {
                let widgets = &mut ui.visuals_mut().widgets;
                widgets.inactive.fg_stroke.color = ACCENT;
                widgets.hovered.fg_stroke.color = ACCENT;
                widgets.active.fg_stroke.color = ACCENT_BRIGHT;
            }
            if ui.add(egui::Checkbox::without_text(&mut value)).changed() {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, value);
                setter.end_set_parameter(param);
            }
        },
    );
}

/// A parameter's current value, beside its name, and the way to type an exact one in.
///
/// The slider below draws no value of its own — [`widgets::ParamSlider`] puts it to the right of the
/// bar, which is what made the rows ragged — and the click-to-type the widget hangs on that text
/// goes with it. The reading has moved up here, so typing into it has too: click the number, type,
/// enter. Escape, or clicking away, leaves the parameter alone.
fn reading<P: Param>(ui: &mut egui::Ui, param: &P, setter: &ParamSetter) {
    // Keyed by the parameter rather than by where its cell landed, so that a row rewrapping under
    // the pointer cannot hand a half-typed value to whatever takes its place.
    let entry = egui::Id::new(("value entry", param.name()));
    let field = entry.with("field");
    // Monospace, so that a value changing under the pointer does not shuffle the name beside it,
    // and no larger than that name: the slider is what the eye goes to, not its caption.
    let font = egui::FontId::monospace(egui::TextStyle::Small.resolve(ui.style()).size);
    let room = ui.available_width();

    let Some(mut typed) = ui.memory(|memory| memory.data.get_temp::<String>(entry)) else {
        let text = param.to_string();
        let width = ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(text.clone(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x
        });
        let response = ui.add(
            egui::Label::new(egui::RichText::new(&text).font(font))
                .sense(egui::Sense::click())
                .truncate(),
        );
        // Only where it has actually been cut short. A tooltip repeating what is already on screen
        // is noise, and every parameter in the window would carry one.
        let response = if width > room {
            response.on_hover_text(text)
        } else {
            response
        };

        if response.clicked() {
            ui.memory_mut(|memory| {
                memory.data.insert_temp(entry, param.to_string());
                // The field itself does not exist until the next frame, which egui allows.
                memory.request_focus(field);
            });
        }
        return;
    };

    let response = ui.add(
        egui::TextEdit::singleline(&mut typed)
            .id(field)
            .desired_width(room)
            .font(font),
    );

    if response.lost_focus() {
        // Enter commits, anything else — escape, or the pointer going elsewhere — abandons it.
        if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            if let Some(normalized) = param.string_to_normalized_value(&typed) {
                setter.begin_set_parameter(param);
                setter.set_parameter_normalized(param, normalized);
                setter.end_set_parameter(param);
            }
        }
        ui.memory_mut(|memory| memory.data.remove::<String>(entry));
    } else {
        ui.memory_mut(|memory| memory.data.insert_temp(entry, typed));
    }
}

/// An enum parameter as one radio button per variant, labelled like [`labelled`].
///
/// Every alternative is named on screen, so the choice can be read and made without being opened or
/// dragged: a set of named modes is not a value you slide along, and reading it as one meant working
/// out which point of the travel you were at.
///
/// The cell spans as many whole columns as the variant names need — a three-way switch with a
/// sentence for each option cannot be squeezed into a knob's column — so a wide one still starts and
/// ends on the grid rather than knocking every cell after it out of line. It never spans more than
/// the row can hold, so a long one wraps within itself instead of off the edge.
///
/// There is no reading in the caption line: unlike a slider, the control already names its own
/// value, and repeating it above would only say the same thing twice.
fn radio<T: Enum + PartialEq + Copy + 'static>(
    ui: &mut egui::Ui,
    label: &str,
    param: &EnumParam<T>,
    setter: &ParamSetter,
    metrics: Metrics,
) {
    let names = T::variants();
    // Out to the right edge of what is on screen, not of the layout: at a large ui scale the row is
    // wider than the window, and a variant beyond the edge cannot be read or clicked. Every one of
    // these sits in the scrolling pane, whose bar is drawn over that edge rather than inside it.
    let room = (ui.clip_rect().right() - ui.max_rect().left() - ui.spacing().scroll.bar_width)
        .min(ui.max_rect().width())
        .max(metrics.span(ui, 1));
    let columns = metrics
        .columns_for(ui, radio_row_width(ui, names))
        .min(metrics.columns_in(ui, room));

    cell(
        ui,
        label,
        columns,
        metrics,
        |_| {},
        |ui| {
            // A wrapping row wraps text by default, which would break a variant's name across two
            // lines rather than move the whole button down to the next one.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

            // Each button is added straight to this row, never inside a scope: a scope cannot say
            // how wide it will be, so the row would lose its chance to wrap.
            let current = param.value();
            let plain = ui.style().clone();
            for (index, name) in names.iter().enumerate() {
                let variant = T::from_index(index);
                let selected = variant == current;
                if selected {
                    let widgets = &mut ui.visuals_mut().widgets;
                    widgets.inactive.fg_stroke.color = ACCENT;
                    widgets.hovered.fg_stroke.color = ACCENT_BRIGHT;
                    widgets.active.fg_stroke.color = ACCENT_BRIGHT;
                }
                let response = ui.add(egui::RadioButton::new(selected, *name));
                if selected {
                    // Or the accent would carry over to every variant drawn after it.
                    ui.set_style(plain.clone());
                }

                if response.clicked() && !selected {
                    setter.begin_set_parameter(param);
                    setter.set_parameter(param, variant);
                    setter.end_set_parameter(param);
                }
            }
        },
    );
}

/// An enum parameter as a drop-down, sized like the control inside a [`labelled`] cell.
///
/// [`radio`] is the better read wherever it fits, because every alternative is named on screen. It
/// stops fitting somewhere around the mod matrix's six sources and ten destinations: sixteen buttons
/// on one line is a wall rather than a choice — nothing marks where one parameter ends and the next
/// begins, and no two slots can be compared because each wraps at a different place. A drop-down
/// costs a click to see the alternatives and buys back a column that reads straight down.
///
/// Unlike [`radio`] the selected variant is not accented. The accent there separates the chosen
/// button from the unchosen ones beside it; a closed box has nothing to be distinguished from, so
/// the colour would only be decoration. It appears in the open list instead, where the comparison
/// actually happens.
fn dropdown<T: Enum + PartialEq + Copy + 'static>(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    param: &EnumParam<T>,
    setter: &ParamSetter,
    metrics: Metrics,
) {
    let names = T::variants();
    let current = param.value();

    egui::ComboBox::from_id_salt(id)
        .width(metrics.control().x)
        .selected_text(names.get(current.to_index()).copied().unwrap_or_default())
        .show_ui(ui, |ui| {
            for (index, name) in names.iter().enumerate() {
                let variant = T::from_index(index);
                let selected = variant == current;
                if selected {
                    ui.visuals_mut().override_text_color = Some(ACCENT);
                }
                let response = ui.selectable_label(selected, *name);
                ui.visuals_mut().override_text_color = None;

                if response.clicked() && !selected {
                    setter.begin_set_parameter(param);
                    setter.set_parameter(param, variant);
                    setter.end_set_parameter(param);
                }
            }
        });
}

/// How wide a row of radio buttons wants to be, by the same arithmetic the widget itself does.
///
/// Measured rather than guessed, because the answer moves with the ui scale and with how long the
/// variant names happen to be — `off` and `24-edo (quarter tones)` sit in the same parameter.
fn radio_row_width(ui: &egui::Ui, names: &[&str]) -> f32 {
    let spacing = ui.spacing();
    let font = egui::TextStyle::Button.resolve(ui.style());

    let variants: f32 = names
        .iter()
        .map(|name| {
            let text = ui.fonts(|fonts| {
                fonts
                    .layout_no_wrap((*name).to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                    .size()
                    .x
            });
            (spacing.icon_width + spacing.icon_spacing + text).max(spacing.interact_size.x)
        })
        .sum();

    variants + spacing.item_spacing.x * names.len().saturating_sub(1) as f32
}

fn knobs(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter, metrics: Metrics) {
    ui.label(egui::RichText::new("knobs").strong());
    // One run of cells rather than two rows of four: the grid is what puts them in columns, and
    // letting it wrap to the window means eight across when there is room for eight.
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "rate", &params.rate, setter, metrics);
        labelled(ui, "crush", &params.crush, setter, metrics);
        labelled(ui, "attack", &params.attack, setter, metrics);
        labelled(ui, "release", &params.release, setter, metrics);
        labelled(ui, "grain size", &params.grain, setter, metrics);
        // Shift moves the grain origin, so with grain size at zero — the sample playing straight
        // through, no granular engine — the knob does nothing whatever it is set to. Grey it out
        // rather than leave it reading "+5400 b/grain" while it is inert.
        dimmed(ui, params.grain.value() > 0, metrics, |ui| {
            labelled(ui, "shift", &params.shift, setter, metrics);
        });
        labelled(ui, "start", &params.start, setter, metrics);
        labelled(ui, "end", &params.end, setter, metrics);
    });

    // The shift curve folds back on the hardware; say so where it is actually visible.
    let raw = params.shift.value() as u16;
    let hardware = shift_bytes(raw, granny_core::curves::CurveMode::HardwareExact);
    let extended = shift_bytes(raw, granny_core::curves::CurveMode::Extended);
    if hardware != extended {
        ui.label(
            egui::RichText::new(format!(
                "shift {raw}: hardware curve gives {hardware:+} b/grain where the table implies \
                 {extended:+} — the avr's 16-bit multiply overflows here"
            ))
            .weak()
            .italics(),
        );
    }
}

fn settings(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter, metrics: Metrics) {
    ui.label(egui::RichText::new("settings").strong());
    ui.horizontal_wrapped(|ui| {
        radio(ui, "note mode", &params.note_mode, setter, metrics);
        toggle(ui, "legato", &params.legato, setter, metrics);
        toggle(ui, "repeat", &params.repeat, setter, metrics);
        toggle(ui, "sync", &params.sync, setter, metrics);
        // All this does is flip the sign of the grain shift, so it goes grey with the shift knob.
        dimmed(ui, params.grain.value() > 0, metrics, |ui| {
            toggle(ui, "random shift", &params.random_shift, setter, metrics);
        });
        toggle(ui, "hold", &params.hold, setter, metrics);
        labelled(ui, "level", &params.level, setter, metrics);
        labelled(
            ui,
            "velocity sens",
            &params.vel_sensitivity,
            setter,
            metrics,
        );
        labelled(ui, "voices", &params.voices, setter, metrics);
        toggle(ui, "hardware cc map", &params.hardware_cc, setter, metrics);
    });
}

/// Say plainly what the sample was detected as, and what note therefore plays it untransposed.
fn root_summary(params: &Arc<MaterParams>, sample: &SampleBuffer) -> egui::RichText {
    if sample.is_empty() {
        return egui::RichText::new("no sample loaded").weak().italics();
    }
    if !params.match_input_pitch.value() {
        return egui::RichText::new(
            "matching off — the sample plays at its recorded speed on b3, as the hardware does",
        )
        .weak()
        .italics();
    }

    let adjust = params.root_adjust.value();
    let text = match sample.detected_root() {
        Some(detection) => format!(
            "detected {} at {:.1} hz ({:.0} % confident) — playing that note gives back the \
             original pitch{}",
            describe_note(detection.note),
            detection.frequency,
            detection.confidence * 100.0,
            if adjust == 0.0 {
                String::new()
            } else {
                format!(", adjusted to {}", describe_note(detection.note + adjust))
            }
        ),
        None => format!(
            "no clear pitch in this sample — rooted on {}, set root adjust by ear",
            describe_note(granny_core::tables::NATIVE_NOTE + adjust)
        ),
    };
    egui::RichText::new(text).weak().italics()
}

fn tuning(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    setter: &ParamSetter,
    executor: &AsyncExecutor<Mater>,
    sample: &SampleBuffer,
    metrics: Metrics,
) {
    ui.label(egui::RichText::new("tuning").strong());
    // The same grid as the sections above, which the long enum names fit by spanning columns rather
    // than by being cut into rows of their own.
    ui.horizontal_wrapped(|ui| {
        toggle(
            ui,
            "match input pitch",
            &params.match_input_pitch,
            setter,
            metrics,
        );
        labelled(ui, "root adjust", &params.root_adjust, setter, metrics);
        radio(ui, "pitch table", &params.pitch_table, setter, metrics);
        radio(ui, "snap", &params.snap, setter, metrics);
        radio(ui, "mpe", &params.mpe_zone, setter, metrics);
        labelled(ui, "mpe bend range", &params.bend_range, setter, metrics);
        labelled(
            ui,
            "midi bend range",
            &params.master_bend_range,
            setter,
            metrics,
        );
        toggle(ui, "follow rpn 0", &params.follow_rpn, setter, metrics);
        toggle(ui, "use scala scale", &params.use_scala, setter, metrics);
    });

    // What the sample was found to be, and therefore what playback is transposed from.
    ui.label(root_summary(params, sample));

    // The scale files are not parameters, so they keep a row of their own under the grid.
    ui.horizontal_wrapped(|ui| {
        if ui.button("load .scl…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("scala scale", &["scl"])
                .pick_file()
            {
                executor.execute_background(Task::LoadScl(path));
            }
        }
        if ui.button("load .kbm…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("scala keyboard map", &["kbm"])
                .pick_file()
            {
                executor.execute_background(Task::LoadKbm(path));
            }
        }
        if ui.button("clear scale").clicked() {
            executor.execute_background(Task::ClearScale);
        }

        let scale = params.scale.snapshot();
        let description = if scale.is_empty() {
            "no scale loaded".to_string()
        } else if scale.kbm_name.is_empty() {
            scale.scl_name.clone()
        } else {
            format!("{} + {}", scale.scl_name, scale.kbm_name)
        };
        ui.label(description);
    });
}

fn mod_matrix(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    setter: &ParamSetter,
    metrics: Metrics,
) {
    ui.label(egui::RichText::new("mod matrix").strong());
    ui.label(
        egui::RichText::new("per-voice, applied on top of the knob values")
            .weak()
            .italics(),
    );
    // Three slots are the same three parameters over again, so they are laid out as a table rather
    // than as three independent rows: the columns are what let one slot be read against the next.
    // Naming them once at the top also frees each cell of the label it would otherwise carry.
    egui::Grid::new("mod matrix")
        .num_columns(4)
        .spacing(egui::vec2(metrics.at(8.0), metrics.at(6.0)))
        .show(ui, |ui| {
            ui.label("");
            for column in ["source", "destination", "depth"] {
                ui.label(egui::RichText::new(column).small());
            }
            ui.end_row();

            for (index, row) in params.mods.iter().enumerate() {
                ui.label(egui::RichText::new(format!("{}", index + 1)).small());
                dropdown(ui, ("mod source", index), &row.source, setter, metrics);
                dropdown(ui, ("mod destination", index), &row.dest, setter, metrics);
                ui.add_sized(
                    metrics.control(),
                    widgets::ParamSlider::for_param(&row.depth, setter),
                );
                ui.end_row();
            }
        });
}

fn fidelity(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter, metrics: Metrics) {
    ui.label(egui::RichText::new("fidelity").strong());
    ui.label(
        egui::RichText::new("defaults reproduce the hardware, including its rough edges")
            .weak()
            .italics(),
    );
    ui.horizontal_wrapped(|ui| {
        radio(ui, "curve maps", &params.curve_mode, setter, metrics);
        toggle(ui, "interpolate", &params.interpolate, setter, metrics);
        toggle(
            ui,
            "block-quantise seeks",
            &params.quantize_seeks,
            setter,
            metrics,
        );
        labelled(ui, "grain fade", &params.grain_fade, setter, metrics);
        toggle(
            ui,
            "resample on load",
            &params.resample_on_load,
            setter,
            metrics,
        );
        toggle(
            ui,
            "normalise on load",
            &params.normalize_on_load,
            setter,
            metrics,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the egui editor, which needs a window to build. `takes_scaling` is what the
    /// real one does: it turns a scale factor down while its window is open.
    struct NoEditor {
        takes_scaling: bool,
    }

    impl Editor for NoEditor {
        fn spawn(
            &self,
            _parent: ParentWindowHandle,
            _context: Arc<dyn GuiContext>,
        ) -> Box<dyn std::any::Any + Send> {
            unreachable!("the test never opens a window")
        }
        fn size(&self) -> (u32, u32) {
            (960, 700)
        }
        fn set_scale_factor(&self, _factor: f32) -> bool {
            self.takes_scaling
        }
        fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}
        fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
        fn param_values_changed(&self) {}
    }

    fn editor_over(takes_scaling: bool) -> (HostDpi, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        let editor = HostDpi {
            inner: Box::new(NoEditor { takes_scaling }),
            shared: shared.clone(),
        };
        (editor, shared)
    }

    #[test]
    fn the_hosts_dpi_scaling_is_taken_and_remembered() {
        let (editor, shared) = editor_over(true);

        assert!(editor.set_scale_factor(2.0));
        assert_eq!(shared.host_dpi.load(Ordering::Relaxed), 2.0);
        assert!(shared.host_dpi_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn a_host_that_never_announces_one_is_not_read_as_announcing_100_percent() {
        // The stored factor starts at the same 1.0 a host scaling by 100 % would send, so only the
        // flag can tell "the host says no scaling" from "the host has said nothing".
        let (_editor, shared) = editor_over(true);

        assert_eq!(shared.host_dpi.load(Ordering::Relaxed), 1.0);
        assert!(!shared.host_dpi_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn a_refused_factor_is_not_remembered() {
        // The window keeps being sized by the factor it was opened with, so believing this one
        // would divide the layout by a number nothing on screen is using.
        let (editor, shared) = editor_over(false);

        assert!(!editor.set_scale_factor(2.0));
        assert_eq!(shared.host_dpi.load(Ordering::Relaxed), 1.0);
        // And a refused one is not something to report either, or the tooltip would claim the host
        // scales by 100 % on the strength of a factor that never took.
        assert!(!shared.host_dpi_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn the_hosts_scaling_does_not_change_how_large_the_interface_looks() {
        // 100 % is the same size on screen whatever the host announces: at 200 % every point is
        // worth two pixels, so the layout works in half as many of them.
        assert_eq!(layout_scale(1.0, 1.0), 1.0);
        assert_eq!(layout_scale(1.0, 2.0), 0.5);
        assert_eq!(layout_scale(2.5, 2.0), 1.25);
        // And a host that announces nonsense is ignored rather than dividing by zero.
        assert_eq!(layout_scale(1.5, 0.0), 1.5);
    }

    /// A host that announced this factor, as against one that has said nothing.
    fn announced(factor: f32) -> HostScale {
        HostScale {
            factor,
            reported: true,
        }
    }

    /// A host like Bitwig, which never calls `set_scale` at all. The factor it leaves behind is the
    /// 1.0 it started at, which is exactly what makes the flag necessary.
    const SILENT: HostScale = HostScale {
        factor: 1.0,
        reported: false,
    };

    #[test]
    fn an_untouched_scale_follows_the_host_and_fills_the_window() {
        let params = MaterParams::new(Arc::new(Shared::default()));

        // The window a host at 200 % makes is twice the size, so the interface has to be too or
        // the difference is left empty. A layout scale of exactly 1 is that interface.
        assert_eq!(ui_scale(&params, announced(2.0), 1.0), 2.0);
        assert_eq!(
            layout_scale(ui_scale(&params, announced(2.0), 1.0), 2.0),
            1.0
        );
        // Including at whatever odd factor a host feels like reporting.
        assert_eq!(
            layout_scale(ui_scale(&params, announced(1.6), 1.0), 1.6),
            1.0
        );
    }

    #[test]
    fn a_silent_host_is_not_taken_for_one_asking_for_100_percent() {
        let params = MaterParams::new(Arc::new(Shared::default()));

        // The desktop's 200 % is the whole answer here: nothing multiplies the points on the way to
        // the screen, so the interface has to be drawn at twice the size rather than divided by it.
        assert_eq!(ui_scale(&params, SILENT, 2.0), 2.0);
        assert_eq!(
            layout_scale(ui_scale(&params, SILENT, 2.0), SILENT.factor),
            2.0
        );
        // And a host that does announce 100 % is believed over the desktop, not overruled by it.
        assert_eq!(ui_scale(&params, announced(1.0), 2.0), 1.0);
    }

    #[test]
    fn a_scale_set_by_hand_outranks_both_of_them() {
        let params = MaterParams::new(Arc::new(Shared::default()));
        params.set_ui_scale(1.5);

        assert_eq!(ui_scale(&params, announced(2.0), 2.0), 1.5);
        assert_eq!(ui_scale(&params, SILENT, 2.0), 1.5);
    }

    #[test]
    fn an_unchosen_window_is_sized_for_the_scale_it_will_be_drawn_at() {
        // The arithmetic `size_for_scale` applies to a window still at the default. A 200 % desktop
        // needs twice the window, or the interface is drawn into half the room it was laid out for.
        assert_eq!(
            rescaled(DEFAULT_WINDOW, 1.0, 2.0),
            egui::vec2(DEFAULT_WINDOW.0 as f32 * 2.0, DEFAULT_WINDOW.1 as f32 * 2.0)
        );
        // And a host that scales for us leaves the layout at 1, which asks for no change at all.
        assert_eq!(
            rescaled(DEFAULT_WINDOW, 1.0, 1.0),
            egui::vec2(DEFAULT_WINDOW.0 as f32, DEFAULT_WINDOW.1 as f32)
        );
    }

    #[test]
    fn a_desktop_scaling_is_held_to_the_steps_the_interface_offers() {
        // Whatever `Xft.dpi` says, this is a size someone has to be able to work at.
        let scale = display_scale();

        assert!(scale >= UI_SCALES[0]);
        assert!(scale <= UI_SCALES[UI_SCALES.len() - 1]);
    }

    #[test]
    fn the_waveform_takes_what_the_controls_leave() {
        let metrics = Metrics { scale: 1.0 };

        // A window with room to spare: the waveform absorbs it instead of leaving it empty.
        assert_eq!(wave_height(1000.0, Some(300.0), metrics), 694.0);
        // One that is already too small: the controls scroll and the waveform keeps its share.
        assert_eq!(wave_height(600.0, Some(560.0), metrics), 240.0);
        // Exactly enough for both is the case the share must not win.
        assert_eq!(wave_height(1000.0, Some(600.0), metrics), 394.0);
        // Before anything under it has been drawn, that share is all there is to go on.
        assert_eq!(wave_height(1000.0, None, metrics), 360.0);
        // And the share is held above a floor however little the window leaves.
        assert_eq!(wave_height(200.0, Some(500.0), metrics), 160.0);
    }

    #[test]
    fn the_waveform_scales_with_the_interface() {
        // The bounds on its share are sizes on screen, so they move with the rest of the layout.
        let metrics = Metrics { scale: 2.0 };

        assert_eq!(wave_height(1000.0, None, metrics), 400.0);
        assert_eq!(wave_height(600.0, Some(560.0), metrics), 320.0);
    }

    #[test]
    fn the_window_follows_the_scale_it_is_holding() {
        // Smaller interface, smaller window: leaving it where it was is what put an empty band
        // along the bottom of it.
        assert_eq!(rescaled((960, 700), 2.0, 1.75), egui::vec2(840.0, 612.5));
        assert_eq!(rescaled((960, 700), 1.0, 2.0), egui::vec2(1920.0, 1400.0));
        assert_eq!(rescaled((960, 700), 1.5, 1.5), egui::vec2(960.0, 700.0));
        // Whatever the window has been dragged to is what gets scaled, not the size it opened at.
        assert_eq!(rescaled((1200, 900), 2.0, 1.0), egui::vec2(600.0, 450.0));
        assert_eq!(rescaled((960, 700), 0.0, 2.0), egui::vec2(960.0, 700.0));
    }

    #[test]
    fn a_scale_set_by_hand_is_not_moved_by_the_host() {
        let params = MaterParams::new(Arc::new(Shared::default()));
        params.set_ui_scale(1.25);

        assert_eq!(ui_scale(&params, announced(2.0), 1.0), 1.25);
        // Which is a smaller interface than the window was sized for. That is the point of asking.
        assert_eq!(
            layout_scale(ui_scale(&params, announced(2.0), 1.0), 2.0),
            0.625
        );
    }
}
