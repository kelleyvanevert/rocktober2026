//! The main window: code editor, console, status bar.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle, RopeExt, TextDecoration, TextDecorationCollection,
};
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rocktober_engine::Session;

use crate::{RunAll, RunBlock, Save, StopAll, blocks, comments};

const FLASH_DURATION: Duration = Duration::from_millis(250);
const MAX_LOG_ENTRIES: usize = 500;

const EXAMPLE: &str = r#"-- cmd-enter       run the selection, or the block under the cursor
-- cmd-shift-enter run everything
-- cmd-.           stop all sound

play(sample("kick.mp3"))

play(fit(sample("kick.mp3"), 500ms))

play(repeat(fit(sample("kick.mp3"), 250ms), 4))
"#;

#[derive(Clone, Copy, PartialEq)]
enum LogKind {
    Info,
    Ran,
    Error,
}

struct LogEntry {
    kind: LogKind,
    text: SharedString,
}

pub struct Workspace {
    path: PathBuf,
    dirty: bool,
    editor: Entity<EditorState>,
    flash: RangeDecorationCollection,
    comments: TextDecorationCollection,
    error: TextDecorationCollection,
    /// Bumped on every flash, so an old flash's timer doesn't clear a newer one.
    flash_generation: u64,
    session: Option<Session>,
    voices: usize,
    log: Vec<LogEntry>,
    log_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// Where `sample("...")` looks for files: next to the code file, then the
/// working directory, each with and without a `samples/` subfolder.
pub fn sample_dirs(path: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("."), PathBuf::from("samples")];
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        dirs.insert(0, dir.join("samples"));
        dirs.insert(0, dir.to_path_buf());
    }
    dirs
}

impl Workspace {
    pub fn new(
        path: PathBuf,
        session: Result<Session, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut log = Vec::new();
        let mut info = |kind, text: String| {
            log.push(LogEntry {
                kind,
                text: text.into(),
            })
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => {
                info(LogKind::Info, format!("opened {}", path.display()));
                text
            }
            Err(_) => {
                info(
                    LogKind::Info,
                    format!("new file {} (cmd-s to save)", path.display()),
                );
                EXAMPLE.to_string()
            }
        };

        let session = match session {
            Ok(s) => {
                info(
                    LogKind::Info,
                    format!(
                        "audio: {} ({} Hz, {} ch)",
                        s.device_name, s.sample_rate, s.channels
                    ),
                );
                Some(s)
            }
            Err(e) => {
                info(LogKind::Error, format!("no audio: {e}"));
                None
            }
        };

        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx);
            state.set_value(text, window, cx);
            state.focus(window, cx);
            state
        });
        let (flash, comments, error) = editor.update(cx, |state, cx| {
            (
                state.create_range_decorations_collection(vec![], cx),
                state.create_decorations_collection(vec![], cx),
                state.create_decorations_collection(vec![], cx),
            )
        });

        let subscriptions = vec![cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.highlight_comments(cx);
                if !this.dirty {
                    this.dirty = true;
                    cx.notify();
                }
            }
        })];

        // The audio thread can't call us, so poll its voice count for the status bar.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    let voices = this.session.as_ref().map_or(0, |s| s.voices());
                    if voices != this.voices {
                        this.voices = voices;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let workspace = Self {
            path,
            dirty: false,
            editor,
            flash,
            comments,
            error,
            flash_generation: 0,
            session,
            voices: 0,
            log,
            log_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        workspace.highlight_comments(cx);
        workspace
    }

    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    /// Replace the whole buffer. (`EditorState::set_value` emits no change event,
    /// so going through here keeps the comment highlighting in sync.)
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.set_value(text.to_string(), window, cx)
        });
        self.highlight_comments(cx);
    }

    /// The console contents, oldest first.
    pub fn log_lines(&self) -> impl Iterator<Item = &str> {
        self.log.iter().map(|entry| entry.text.as_str())
    }

    fn run_block(&mut self, _: &RunBlock, _: &mut Window, cx: &mut Context<Self>) {
        let state = self.editor.read(cx);
        let text = state.value().to_string();
        let selection = state.selected_range();
        let range = if selection.is_empty() {
            blocks::block_at(&text, state.cursor())
        } else {
            selection
        };
        self.run(&text, range, cx);
    }

    fn run_all(&mut self, _: &RunAll, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let len = text.len();
        self.run(&text, 0..len, cx);
    }

    fn stop_all(&mut self, _: &StopAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &mut self.session {
            session.stop_all();
            self.push_log(LogKind::Info, "stop".into(), cx);
        }
    }

    fn save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value();
        match std::fs::write(&self.path, text.as_str()) {
            Ok(()) => {
                self.dirty = false;
                self.push_log(LogKind::Info, format!("saved {}", self.path.display()), cx);
            }
            Err(e) => self.push_log(LogKind::Error, format!("save failed: {e}"), cx),
        }
    }

    fn run(&mut self, text: &str, range: Range<usize>, cx: &mut Context<Self>) {
        let code = &text[range.clone()];
        if code.trim().is_empty() {
            return;
        }
        self.flash(range.clone(), cx);

        let Some(session) = &mut self.session else {
            self.push_log(LogKind::Error, "no audio device".into(), cx);
            return;
        };
        let result = session.eval(code);

        let mut summary = code.trim().lines().next().unwrap_or_default().to_string();
        if code.trim().lines().nth(1).is_some() {
            summary.push_str(" …");
        }

        match result {
            Ok(()) => {
                self.set_error(None, cx);
                self.push_log(LogKind::Ran, summary, cx);
            }
            Err(e) => {
                let at = range.start + e.pos;
                let pos = self.editor.read(cx).text().offset_to_position(at);
                self.set_error(Some(at), cx);
                self.push_log(
                    LogKind::Error,
                    format!("{}:{}: {}", pos.line + 1, pos.character + 1, e.msg),
                    cx,
                );
            }
        }
    }

    /// Briefly highlight the code that was just run.
    fn flash(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let color = cx.theme().primary.opacity(0.35);
        self.flash.set(
            vec![
                RangeDecoration::new(range)
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(color),
            ],
            cx,
        );
        self.flash_generation += 1;
        let generation = self.flash_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this.flash_generation == generation {
                    this.flash.clear(cx);
                }
            });
        })
        .detach();
    }

    /// Mute comments. Recomputed from scratch on every edit, which is plenty fast
    /// for files of livecoding size.
    fn highlight_comments(&self, cx: &mut Context<Self>) {
        let style = HighlightStyle {
            color: Some(cx.theme().muted_foreground),
            ..Default::default()
        };
        let text = self.editor.read(cx).value();
        let decorations = comments::comment_ranges(&text)
            .into_iter()
            .map(|range| TextDecoration::new(range, style))
            .collect();
        self.comments.set(decorations, cx);
    }

    /// Show (or clear) an error squiggle in the editor. (The editor's own
    /// diagnostics are only drawn when a syntax highlighter is active.)
    fn set_error(&mut self, at: Option<usize>, cx: &mut Context<Self>) {
        let decorations = at.map(|at| {
            let len = self.editor.read(cx).text().len();
            let style = HighlightStyle {
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(cx.theme().danger),
                    wavy: true,
                }),
                ..Default::default()
            };
            TextDecoration::new(at..(at + 1).min(len), style)
        });
        self.error.set(decorations.into_iter().collect(), cx);
    }

    fn push_log(&mut self, kind: LogKind, text: String, cx: &mut Context<Self>) {
        self.log.push(LogEntry {
            kind,
            text: text.into(),
        });
        if self.log.len() > MAX_LOG_ENTRIES {
            self.log.drain(..self.log.len() - MAX_LOG_ENTRIES);
        }
        self.log_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let file_name = self
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let voices = match self.voices {
            1 => "1 voice".to_string(),
            n => format!("{n} voices"),
        };

        h_flex()
            .flex_shrink_0()
            .gap_3()
            .px_3()
            .py_0p5()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(format!("{file_name}{}", if self.dirty { " •" } else { "" }))
            .child(div().flex_1())
            .child(
                div()
                    .when(self.voices > 0, |this| this.text_color(theme.success))
                    .child(voices),
            )
    }

    fn render_log(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        div()
            .id("log")
            .h(px(150.))
            .flex_shrink_0()
            .overflow_y_scroll()
            .track_scroll(&self.log_scroll)
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .font_family(theme.mono_font_family.clone())
            .text_xs()
            .children(self.log.iter().map(|entry| {
                let (prefix, color) = match entry.kind {
                    LogKind::Info => ("  ", theme.muted_foreground),
                    LogKind::Ran => ("▶ ", theme.foreground),
                    LogKind::Error => ("✗ ", theme.danger),
                };
                div()
                    .text_color(color)
                    .child(format!("{prefix}{}", entry.text))
            }))
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let log = self.render_log(cx);
        let status_bar = self.render_status_bar(cx);
        let theme = cx.theme();
        v_flex()
            .key_context("Workspace")
            .on_action(cx.listener(Self::run_block))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::stop_all))
            .on_action(cx.listener(Self::save))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .child(Editor::new(&self.editor).appearance(false).h_full()),
            )
            .child(log)
            .child(status_bar)
    }
}
