//! The editor: a waveform you can work on directly, with the parameters underneath.
//!
//! The hardware's interface is four knobs, a two-page toggle and a four-digit display. That makes
//! sense when you are holding it. On screen, with sixteen independent voices crawling over the
//! sample at once, the waveform is the interface: start and end are handles you drag, the grain
//! window is shaded, and every sounding voice draws its own playhead.

use granny_core::curves::{grain_ms, shift_bytes};
use granny_core::pitch::describe_note;
use granny_core::sample::SampleBuffer;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, resizable_window::ResizableWindow, widgets};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::params::{describe_sample, MaterParams, UI_SCALES};
use crate::project;
use crate::shared::{Shared, PLAYHEAD_IDLE};
use crate::{Mater, Task};

/// The one accent colour: handles, playheads, filled sliders, anything switched on.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(255, 146, 38);
/// The same hue lifted, for hover and for text that has to stay legible on the accent.
const ACCENT_BRIGHT: egui::Color32 = egui::Color32::from_rgb(255, 187, 112);
/// And dropped, for fills that sit behind text — slider bars, selected ranges.
const ACCENT_FILL: egui::Color32 = egui::Color32::from_rgb(122, 60, 10);

/// Fraction of the window the waveform gets, and the bounds it is held within.
const WAVE_FRACTION: f32 = 0.4;
const WAVE_MIN_HEIGHT: f32 = 160.0;
const WAVE_MAX_HEIGHT: f32 = 360.0;
/// However large the interface is drawn, the waveform never takes more of the window than this.
const WAVE_HEIGHT_LIMIT: f32 = 0.55;
/// How close to a handle the pointer must be to grab it.
const HANDLE_GRAB_PX: f32 = 10.0;
/// Width of one labelled parameter cell, so the rows line up into columns.
const CELL_WIDTH: f32 = 190.0;
const CELL_HEIGHT: f32 = 42.0;
/// Height of the control inside a cell.
const CONTROL_HEIGHT: f32 = 20.0;
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
    /// A labelled parameter cell.
    fn cell(self) -> egui::Vec2 {
        egui::vec2(CELL_WIDTH * self.scale, CELL_HEIGHT * self.scale)
    }

    /// The control that sits inside one.
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
}

pub fn create(
    params: Arc<MaterParams>,
    shared: Arc<Shared>,
    async_executor: AsyncExecutor<Mater>,
) -> Option<Box<dyn Editor>> {
    let egui_state = params.editor_state.clone();

    create_egui_editor(
        egui_state.clone(),
        EditorState::default(),
        // A reopened window is a fresh context with egui's own style, so ask for ours again.
        |_, state: &mut EditorState| state.styled_for = None,
        move |ctx, setter, state| {
            // Anything the audio thread displaced is dropped here, on the main thread.
            shared.collect_garbage();
            handle_dropped_files(ctx, setter, &shared, &async_executor);

            let scale = params.ui_scale();
            if state.styled_for != Some(scale) {
                apply_style(ctx, scale);
                state.styled_for = Some(scale);
            }
            let metrics = Metrics { scale };

            ResizableWindow::new("mater")
                .min_size(egui::Vec2::new(640.0, 480.0))
                .show(ctx, egui_state.as_ref(), |ui| {
                    egui::Frame::NONE
                        .inner_margin(metrics.padding())
                        .show(ui, |ui| {
                            let sample = shared.editor_sample.lock().clone();

                            header(
                                ui,
                                &params,
                                &shared,
                                setter,
                                &async_executor,
                                &sample,
                                metrics,
                            );
                            ui.add_space(metrics.at(6.0));
                            waveform(ui, &params, &shared, setter, state, &sample, metrics);
                            ui.add_space(metrics.at(6.0));

                            egui::ScrollArea::vertical().show(ui, |ui| {
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
                        });
                });
        },
    )
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

/// egui's default text styles, scaled.
fn text_styles(scale: f32) -> BTreeMap<egui::TextStyle, egui::FontId> {
    use egui::FontFamily::{Monospace, Proportional};
    use egui::{FontId, TextStyle};

    [
        (TextStyle::Small, FontId::new(9.0 * scale, Proportional)),
        (TextStyle::Body, FontId::new(12.5 * scale, Proportional)),
        (TextStyle::Button, FontId::new(12.5 * scale, Proportional)),
        (TextStyle::Heading, FontId::new(18.0 * scale, Proportional)),
        (TextStyle::Monospace, FontId::new(12.0 * scale, Monospace)),
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
    setter: &ParamSetter,
    executor: &AsyncExecutor<Mater>,
    sample: &SampleBuffer,
    metrics: Metrics,
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
            scale_control(ui, params, metrics);
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
fn scale_control(ui: &mut egui::Ui, params: &Arc<MaterParams>, metrics: Metrics) {
    let scale = metrics.scale;
    // The nearest step, so a state saved by a future version with other steps still lands somewhere.
    let step = UI_SCALES
        .iter()
        .position(|&candidate| candidate >= scale)
        .unwrap_or(UI_SCALES.len() - 1);

    let larger = ui.add_enabled(step + 1 < UI_SCALES.len(), egui::Button::new("+"));
    if larger.clicked() {
        params.set_ui_scale(UI_SCALES[step + 1]);
    }

    ui.label(egui::RichText::new(format!("{:.0} %", scale * 100.0)).monospace());

    let smaller = ui.add_enabled(step > 0, egui::Button::new("−"));
    if smaller.clicked() {
        params.set_ui_scale(UI_SCALES[step - 1]);
    }

    ui.label(egui::RichText::new("ui scale").weak());
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
    let available = ui.available_height();
    let height = (available * WAVE_FRACTION)
        .clamp(metrics.at(WAVE_MIN_HEIGHT), metrics.at(WAVE_MAX_HEIGHT))
        .min(available * WAVE_HEIGHT_LIMIT);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();

    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);

    if sample.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no sample — click “load sample…” or drop a file here",
            egui::FontId::proportional(metrics.at(15.0)),
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
            egui::FontId::monospace(metrics.at(11.0)),
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

/// One parameter in a fixed-width cell, so wrapped rows line up as columns.
fn labelled<'a>(
    ui: &mut egui::Ui,
    label: &str,
    param: &'a impl Param,
    setter: &'a ParamSetter,
    metrics: Metrics,
) {
    ui.allocate_ui(metrics.cell(), |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).small());
            ui.add_sized(
                metrics.control(),
                widgets::ParamSlider::for_param(param, setter),
            );
        });
    });
}

/// A switch in the same cell shape as [`labelled`].
///
/// On and off are states, not values you slide between, so they get a checkbox — and the accent
/// colour, so a row of them can be read at a glance.
fn toggle(
    ui: &mut egui::Ui,
    label: &str,
    param: &BoolParam,
    setter: &ParamSetter,
    metrics: Metrics,
) {
    ui.allocate_ui(metrics.cell(), |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).small());

            let mut value = param.value();
            let response = ui
                .allocate_ui_with_layout(
                    metrics.control(),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        // A checkbox is narrow; the cell still has to hold its column's width.
                        ui.set_min_size(metrics.control());
                        if value {
                            let widgets = &mut ui.visuals_mut().widgets;
                            widgets.inactive.fg_stroke.color = ACCENT;
                            widgets.hovered.fg_stroke.color = ACCENT_BRIGHT;
                            widgets.active.fg_stroke.color = ACCENT_BRIGHT;
                        }
                        let text = if value { "on" } else { "off" };
                        ui.add(egui::Checkbox::new(&mut value, text))
                    },
                )
                .inner;

            if response.changed() {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, value);
                setter.end_set_parameter(param);
            }
        });
    });
}

/// An enum parameter as one radio button per variant, in the same cell shape as [`labelled`].
///
/// For a short list of alternatives this says what the choice is without being opened or dragged: a
/// two-way switch between named modes is not a value you slide along, and reading it as one meant
/// working out which end of the travel you were at.
fn radio<T: Enum + PartialEq + Copy + 'static>(
    ui: &mut egui::Ui,
    label: &str,
    param: &EnumParam<T>,
    setter: &ParamSetter,
    metrics: Metrics,
) {
    ui.allocate_ui(metrics.cell(), |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).small());

            let current = param.value();
            ui.allocate_ui_with_layout(
                metrics.control(),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    // As with a checkbox, the cell still has to hold its column's width.
                    ui.set_min_size(metrics.control());

                    for (index, name) in T::variants().iter().enumerate() {
                        let variant = T::from_index(index);
                        let selected = variant == current;
                        // Scoped, or the accent applied to the selected variant would carry over
                        // to every variant drawn after it.
                        let response = ui
                            .scope(|ui| {
                                if selected {
                                    let widgets = &mut ui.visuals_mut().widgets;
                                    widgets.inactive.fg_stroke.color = ACCENT;
                                    widgets.hovered.fg_stroke.color = ACCENT_BRIGHT;
                                    widgets.active.fg_stroke.color = ACCENT_BRIGHT;
                                }
                                ui.radio(selected, *name)
                            })
                            .inner;

                        if response.clicked() && !selected {
                            setter.begin_set_parameter(param);
                            setter.set_parameter(param, variant);
                            setter.end_set_parameter(param);
                        }
                    }
                },
            );
        });
    });
}

fn knobs(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter, metrics: Metrics) {
    ui.label(egui::RichText::new("knobs").strong());
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "rate", &params.rate, setter, metrics);
        labelled(ui, "crush", &params.crush, setter, metrics);
        labelled(ui, "attack", &params.attack, setter, metrics);
        labelled(ui, "release", &params.release, setter, metrics);
    });
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "grain size", &params.grain, setter, metrics);
        labelled(ui, "shift", &params.shift, setter, metrics);
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
        toggle(ui, "random shift", &params.random_shift, setter, metrics);
    });
    ui.horizontal_wrapped(|ui| {
        toggle(ui, "hold", &params.hold, setter, metrics);
        labelled(ui, "level", &params.level, setter, metrics);
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
    ui.horizontal_wrapped(|ui| {
        toggle(
            ui,
            "match input pitch",
            &params.match_input_pitch,
            setter,
            metrics,
        );
        labelled(ui, "root adjust", &params.root_adjust, setter, metrics);
        labelled(ui, "pitch table", &params.pitch_table, setter, metrics);
        labelled(ui, "snap", &params.snap, setter, metrics);
    });
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "mpe", &params.mpe_zone, setter, metrics);
        labelled(ui, "mpe bend range", &params.bend_range, setter, metrics);
        labelled(
            ui,
            "midi bend range",
            &params.master_bend_range,
            setter,
            metrics,
        );
        toggle(ui, "follow rpn 0", &params.follow_rpn, setter, metrics);
    });

    // What the sample was found to be, and therefore what playback is transposed from.
    ui.label(root_summary(params, sample));

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
        toggle(ui, "use scala scale", &params.use_scala, setter, metrics);
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
    for (index, row) in params.mods.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{}.", index + 1));
            ui.add_sized(
                metrics.control(),
                widgets::ParamSlider::for_param(&row.source, setter),
            );
            ui.label("→");
            ui.add_sized(
                metrics.control(),
                widgets::ParamSlider::for_param(&row.dest, setter),
            );
            ui.add_sized(
                metrics.control(),
                widgets::ParamSlider::for_param(&row.depth, setter),
            );
        });
    }
}

fn fidelity(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter, metrics: Metrics) {
    ui.label(egui::RichText::new("fidelity").strong());
    ui.label(
        egui::RichText::new("defaults reproduce the hardware, including its rough edges")
            .weak()
            .italics(),
    );
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "curve maps", &params.curve_mode, setter, metrics);
        toggle(ui, "interpolate", &params.interpolate, setter, metrics);
        toggle(
            ui,
            "block-quantise seeks",
            &params.quantize_seeks,
            setter,
            metrics,
        );
        labelled(ui, "grain fade", &params.grain_fade, setter, metrics);
    });
    ui.horizontal_wrapped(|ui| {
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
