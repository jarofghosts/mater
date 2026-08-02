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
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::params::{describe_sample, MaterParams};
use crate::shared::{Shared, PLAYHEAD_IDLE};
use crate::{Mater, Task};

/// Fraction of the window the waveform gets, and the bounds it is held within.
const WAVE_FRACTION: f32 = 0.4;
const WAVE_MIN_HEIGHT: f32 = 160.0;
const WAVE_MAX_HEIGHT: f32 = 360.0;
/// How close to a handle the pointer must be to grab it.
const HANDLE_GRAB_PX: f32 = 10.0;
/// Width of one labelled parameter cell, so the rows line up into columns.
const CELL_WIDTH: f32 = 190.0;
const CELL_HEIGHT: f32 = 42.0;

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
        |_, _| {},
        move |ctx, setter, state| {
            // Anything the audio thread displaced is dropped here, on the main thread.
            shared.collect_garbage();
            handle_dropped_files(ctx, &async_executor);

            ResizableWindow::new("mater")
                .min_size(egui::Vec2::new(640.0, 480.0))
                .show(ctx, egui_state.as_ref(), |ui| {
                    let sample = shared.editor_sample.lock().clone();

                    header(ui, &params, &shared, &async_executor, &sample);
                    ui.add_space(6.0);
                    waveform(ui, &params, &shared, setter, state, &sample);
                    ui.add_space(6.0);

                    egui::ScrollArea::vertical().show(ui, |ui| {
                        knobs(ui, &params, setter);
                        ui.separator();
                        settings(ui, &params, setter);
                        ui.separator();
                        tuning(ui, &params, setter, &async_executor, &sample);
                        ui.separator();
                        mod_matrix(ui, &params, setter);
                        ui.separator();
                        fidelity(ui, &params, setter);
                    });
                });
        },
    )
}

fn handle_dropped_files(ctx: &egui::Context, executor: &AsyncExecutor<Mater>) {
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
            _ => executor.execute_background(Task::LoadSample(path)),
        }
    }
}

fn header(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    shared: &Arc<Shared>,
    executor: &AsyncExecutor<Mater>,
    sample: &SampleBuffer,
) {
    ui.horizontal(|ui| {
        ui.heading("Mater");
        ui.label(egui::RichText::new("microGranny 2.5").weak());
        ui.separator();

        if ui.button("Load sample…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter(
                    "Audio",
                    &["wav", "aiff", "aif", "flac", "mp3", "ogg", "m4a", "caf"],
                )
                .pick_file()
            {
                executor.execute_background(Task::LoadSample(path));
            }
        }

        let path = params.sample.path();
        if !path.is_empty() && ui.button("Reload").clicked() {
            executor.execute_background(Task::LoadSample(path.into()));
        }

        ui.label(describe_sample(sample));
    });

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(shared.status()).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new("drop an audio, .scl or .kbm file anywhere").weak());
        });
    });
}

/// Draw the waveform, its handles, the grain window and every live playhead.
fn waveform(
    ui: &mut egui::Ui,
    params: &Arc<MaterParams>,
    shared: &Arc<Shared>,
    setter: &ParamSetter,
    state: &mut EditorState,
    sample: &SampleBuffer,
) {
    let height = (ui.available_height() * WAVE_FRACTION).clamp(WAVE_MIN_HEIGHT, WAVE_MAX_HEIGHT);
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
            "no sample — click “Load sample…” or drop a file here",
            egui::FontId::proportional(15.0),
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
        painter.rect_filled(
            grain_rect,
            0.0,
            visuals.selection.bg_fill.linear_multiply(0.25),
        );
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
            egui::Stroke::new(1.5, visuals.selection.bg_fill),
        );
    }

    // Handles.
    for (fraction, label) in [(start_fraction, "S"), (end_fraction, "E")] {
        let x = x_at(fraction);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0, visuals.hyperlink_color),
        );
        painter.text(
            egui::pos2(x + 3.0, rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::monospace(11.0),
            visuals.hyperlink_color,
        );
    }

    // Dragging. Grab whichever handle is nearest when the drag begins, then follow the pointer.
    if response.drag_started() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let to_start = (pointer.x - x_at(start_fraction)).abs();
            let to_end = (pointer.x - x_at(end_fraction)).abs();
            state.drag = if to_start.min(to_end) > HANDLE_GRAB_PX {
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
fn labelled<'a>(ui: &mut egui::Ui, label: &str, param: &'a impl Param, setter: &'a ParamSetter) {
    ui.allocate_ui(egui::vec2(CELL_WIDTH, CELL_HEIGHT), |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).small());
            ui.add_sized(
                egui::vec2(CELL_WIDTH, 20.0),
                widgets::ParamSlider::for_param(param, setter),
            );
        });
    });
}

fn knobs(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter) {
    ui.label(egui::RichText::new("Knobs").strong());
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Rate", &params.rate, setter);
        labelled(ui, "Crush", &params.crush, setter);
        labelled(ui, "Attack", &params.attack, setter);
        labelled(ui, "Release", &params.release, setter);
    });
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Grain size", &params.grain, setter);
        labelled(ui, "Shift", &params.shift, setter);
        labelled(ui, "Start", &params.start, setter);
        labelled(ui, "End", &params.end, setter);
    });

    // The shift curve folds back on the hardware; say so where it is actually visible.
    let raw = params.shift.value() as u16;
    let hardware = shift_bytes(raw, granny_core::curves::CurveMode::HardwareExact);
    let extended = shift_bytes(raw, granny_core::curves::CurveMode::Extended);
    if hardware != extended {
        ui.label(
            egui::RichText::new(format!(
                "shift {raw}: hardware curve gives {hardware:+} B/grain where the table implies \
                 {extended:+} — the AVR's 16-bit multiply overflows here"
            ))
            .weak()
            .italics(),
        );
    }
}

fn settings(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter) {
    ui.label(egui::RichText::new("Settings").strong());
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Note mode", &params.note_mode, setter);
        labelled(ui, "Legato", &params.legato, setter);
        labelled(ui, "Repeat", &params.repeat, setter);
        labelled(ui, "Sync", &params.sync, setter);
        labelled(ui, "Random shift", &params.random_shift, setter);
    });
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Hold", &params.hold, setter);
        labelled(ui, "Level", &params.level, setter);
        labelled(ui, "Voices", &params.voices, setter);
        labelled(ui, "Hardware CC map", &params.hardware_cc, setter);
    });
}

/// Say plainly what the sample was detected as, and what note therefore plays it untransposed.
fn root_summary(params: &Arc<MaterParams>, sample: &SampleBuffer) -> egui::RichText {
    if sample.is_empty() {
        return egui::RichText::new("no sample loaded").weak().italics();
    }
    if !params.match_input_pitch.value() {
        return egui::RichText::new(
            "matching off — the sample plays at its recorded speed on B3, as the hardware does",
        )
        .weak()
        .italics();
    }

    let adjust = params.root_adjust.value();
    let text = match sample.detected_root() {
        Some(detection) => format!(
            "detected {} at {:.1} Hz ({:.0} % confident) — playing that note gives back the \
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
            "no clear pitch in this sample — rooted on {}, set Root adjust by ear",
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
) {
    ui.label(egui::RichText::new("Tuning").strong());
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Match input pitch", &params.match_input_pitch, setter);
        labelled(ui, "Root adjust", &params.root_adjust, setter);
        labelled(ui, "Pitch table", &params.pitch_table, setter);
        labelled(ui, "Snap", &params.snap, setter);
    });
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "MPE", &params.mpe_zone, setter);
        labelled(ui, "Bend range", &params.bend_range, setter);
        labelled(ui, "Follow RPN 0", &params.follow_rpn, setter);
    });

    // What the sample was found to be, and therefore what playback is transposed from.
    ui.label(root_summary(params, sample));

    ui.horizontal_wrapped(|ui| {
        if ui.button("Load .scl…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Scala scale", &["scl"])
                .pick_file()
            {
                executor.execute_background(Task::LoadScl(path));
            }
        }
        if ui.button("Load .kbm…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Scala keyboard map", &["kbm"])
                .pick_file()
            {
                executor.execute_background(Task::LoadKbm(path));
            }
        }
        if ui.button("Clear scale").clicked() {
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
        ui.add_sized(
            egui::vec2(CELL_WIDTH, 20.0),
            widgets::ParamSlider::for_param(&params.use_scala, setter),
        );
    });
}

fn mod_matrix(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter) {
    ui.label(egui::RichText::new("Mod matrix").strong());
    ui.label(
        egui::RichText::new("per-voice, applied on top of the knob values")
            .weak()
            .italics(),
    );
    for (index, row) in params.mods.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{}.", index + 1));
            ui.add_sized(
                egui::vec2(CELL_WIDTH, 20.0),
                widgets::ParamSlider::for_param(&row.source, setter),
            );
            ui.label("→");
            ui.add_sized(
                egui::vec2(CELL_WIDTH, 20.0),
                widgets::ParamSlider::for_param(&row.dest, setter),
            );
            ui.add_sized(
                egui::vec2(CELL_WIDTH, 20.0),
                widgets::ParamSlider::for_param(&row.depth, setter),
            );
        });
    }
}

fn fidelity(ui: &mut egui::Ui, params: &Arc<MaterParams>, setter: &ParamSetter) {
    ui.label(egui::RichText::new("Fidelity").strong());
    ui.label(
        egui::RichText::new("defaults reproduce the hardware, including its rough edges")
            .weak()
            .italics(),
    );
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Curve maps", &params.curve_mode, setter);
        labelled(ui, "Interpolate", &params.interpolate, setter);
        labelled(ui, "Block-quantise seeks", &params.quantize_seeks, setter);
        labelled(ui, "Grain fade", &params.grain_fade, setter);
    });
    ui.horizontal_wrapped(|ui| {
        labelled(ui, "Resample on load", &params.resample_on_load, setter);
        labelled(ui, "Normalise on load", &params.normalize_on_load, setter);
    });
}
