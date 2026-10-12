// The editor: a status line, then two columns: the code (left) and the docs (right).

use crate::edit;
use crate::keys::EditingShortcuts;
use crate::lang::{self, Program};
use crate::{Status, VstMidiParams};
use nih_plug::prelude::Editor;
use nih_plug_egui::egui::text::{CCursor, CCursorRange};
use nih_plug_egui::egui::{self, Align, Color32, Key, Layout, Modifiers, RichText, ScrollArea, TextEdit, Ui};
use nih_plug_egui::{create_egui_editor, resizable_window::ResizableWindow};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    /// The parse error for the current code, if any. The last good program keeps playing.
    error: Option<String>,
    /// Whether we've parsed the code since the editor opened.
    checked: bool,
}

pub fn create(
    params: Arc<VstMidiParams>,
    inbox: Arc<Mutex<Option<Program>>>,
    status: Arc<Status>,
) -> Option<Box<dyn Editor>> {
    let egui_state = params.editor_state.clone();
    create_egui_editor(
        params.editor_state.clone(),
        State::default(),
        |_, _| {},
        move |ctx, _setter, state| {
            ResizableWindow::new("vstmidi").min_size(egui::vec2(480.0, 280.0)).show(ctx, &egui_state, |ui| {
                status_line(ui, &status);
                ui.separator();
                ui.columns(2, |cols| {
                    code_column(&mut cols[0], &params, &inbox, state);
                    ScrollArea::vertical().id_salt("docs").auto_shrink([false; 2]).show(&mut cols[1], docs);
                });
            });
            // Keep the position display moving.
            ctx.request_repaint();
        },
    )
    .map(|editor| Box::new(EditingShortcuts(editor)) as Box<dyn Editor>)
}

fn status_line(ui: &mut Ui, status: &Status) {
    let time_sig = status.time_sig.load(Ordering::Relaxed);
    let tempo = status.tempo.load(Ordering::Relaxed);
    let text = if status.playing.load(Ordering::Relaxed) {
        format!(
            "▶  {}.{}.{}",
            status.bar.load(Ordering::Relaxed),
            status.beat.load(Ordering::Relaxed),
            status.sixteenth.load(Ordering::Relaxed),
        )
    } else {
        "■  stopped".to_string()
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).monospace().strong());
        ui.label(RichText::new(format!("{tempo:.1} bpm  {}/{}", time_sig >> 16, time_sig & 0xffff)).monospace().weak());
    });
}

fn code_column(ui: &mut Ui, params: &VstMidiParams, inbox: &Mutex<Option<Program>>, state: &mut State) {
    // Bottom-up, so the error line gets its space first and the editor takes the rest.
    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        match &state.error {
            Some(error) => ui.label(RichText::new(error).monospace().color(Color32::LIGHT_RED)),
            None => ui.label(RichText::new("ok").monospace().weak()),
        };
        ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
            let mut code = params.code.write().unwrap();
            // Taken before the editor runs; it ignores Cmd+/ anyway.
            let toggle_comment = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Slash));
            let mut output = ScrollArea::vertical()
                .id_salt("code")
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    TextEdit::multiline(&mut *code)
                        .code_editor()
                        .lock_focus(true)
                        .desired_width(f32::INFINITY)
                        .min_size(ui.available_size())
                        .show(ui)
                })
                .inner;
            let mut edited = output.response.changed();

            if toggle_comment
                && output.response.has_focus()
                && let Some(range) = output.state.cursor.char_range()
            {
                let [a, b] = range.sorted();
                let (new_code, selection) = edit::toggle_comment(&code, a.index..b.index);
                *code = new_code;
                let range = CCursorRange::two(CCursor::new(selection.start), CCursor::new(selection.end));
                output.state.cursor.set_char_range(Some(range));
                output.state.store(ui.ctx(), output.response.id);
                edited = true;
            }

            if edited || !state.checked {
                state.checked = true;
                match lang::parse(&code) {
                    Ok(program) => {
                        *inbox.lock().unwrap() = Some(program);
                        state.error = None;
                    }
                    Err(e) => state.error = Some(e.to_string()),
                }
            }
        });
    });
}

fn docs(ui: &mut Ui) {
    ui.heading("vstmidi");
    ui.label("The code runs once every sixteenth note, in sync with the host's transport.");

    section(ui, "every N { … }");
    ui.label("Runs its block every N sixteenths, counted from the start of the song.");
    example(ui, "every 4 { note c3 }      -- every beat\nevery 16 { note c2 }     -- every bar (in 4/4)");

    section(ui, "at bar.beat.sixteenth { … }");
    ui.label(
        "Runs its block whenever the position matches. Counted from 0, so Ableton's 1.1.1 is \
         0.0.0 (the status line above shows it this way). Missing parts match anything.",
    );
    example(
        ui,
        "at 0.0.0 { … }     -- the very start\n\
         at x.0.0 { … }     -- each downbeat\n\
         at x.1|3.0 { … }   -- beats 1 and 3, | means \"or\"\n\
         at x.y.2 { … }     -- the \"and\" of each beat",
    );
    ui.label(
        "A letter stands for any number from 0 up, like n in CSS's nth-child, and you can \
         multiply it and add to it (no spaces). The same letter twice must be the same number.",
    );
    example(
        ui,
        "at 4x.0.0 { … }    -- every 4th bar: 0, 4, 8, …\n\
         at 4x.y.0 { … }    -- every beat of those bars\n\
         at 4x.x.0 { … }    -- 0.0.0, 4.1.0, 8.2.0, 12.3.0\n\
         at x.2y+1.0 { … }  -- the odd beats\n\
         at x.x.1 { … }     -- 1.1.1 and 2.2.1, not 1.2.1",
    );

    section(ui, "note NOTE [dur N] [vel N] [ch N]");
    ui.label(
        "Plays a note: a name like c3, c#3 or db3 (c3 = 60, as in Ableton) or a MIDI number. \
         dur is in sixteenths (default 1), vel is 1–127 (default 100), ch is 1–16 (default 1).",
    );
    example(ui, "note c3\nnote f#1 vel 70\nnote 36 dur 4 ch 10");

    section(ui, "Numbers and random numbers");
    ui.label(
        "Anywhere a note takes a number you can calculate with + - * and note names. rN is a \
         random number from 0 to N-1, picked anew for every note.",
    );
    example(
        ui,
        "note c3+7               -- g3\n\
         note c3 + r12           -- a random note in the octave\n\
         note c2+r3*12           -- c2, c3 or c4\n\
         note d1 vel 1+r127      -- a random velocity",
    );
    ui.label(
        "A velocity of 0 means the note isn't played, so r2*100 is a coin flip and r4*40 \
         plays 3 out of 4 times at random velocities. Results outside 0–127 aren't played either.",
    );
    example(ui, "at x.y.3 { note d1 vel r2*30 }   -- ghost snares, half the time");

    section(ui, "Nesting");
    ui.label("Blocks can hold more every's and at's; all of them must match.");
    example(ui, "at x.0 {\n  every 32 { note c3 }  -- beat 0 of every other bar\n}");

    section(ui, "Comments");
    ui.label("Cmd+/ comments out the selected lines, or uncomments them.");
    example(ui, "-- from two dashes to the end of the line");

    section(ui, "In Ableton");
    ui.label(
        "Put vstmidi on its own MIDI track. On the track with the instrument, set \
         MIDI From to that track and choose vstmidi in the second dropdown, then set \
         Monitor to In. With a Drum Rack: c1 is the kick, d1 the snare, f#1 the closed hi-hat.",
    );
    ui.add_space(8.0);
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).monospace().strong());
}

fn example(ui: &mut Ui, code: &str) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(code).monospace());
    });
}
