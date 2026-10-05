//! The main window: the code editor on the left; documentation, the
//! resource editor and the log on the right; a status bar below.

use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle, RopeExt, TextDecoration, TextDecorationCollection,
};
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rocktober_engine::Session;
use rocktober_engine::bundle::{self, Bundle};
use rocktober_engine::resource::{self, ResourceKind, ResourceRef};
use rocktober_engine::spec::Doc;

use crate::docs::{self, Lookup};
use crate::envelope_editor::EnvelopeEditor;
use crate::modulation_editor::ModulationEditor;
use crate::sample_editor::{OverviewCache, SampleEditor};
use crate::{
    ResourceEvent, RunAll, RunBlock, Save, StopAll, StopBlock, ToggleComment, ToggleRecording,
    blocks, comments,
};

const FLASH_DURATION: Duration = Duration::from_millis(250);
/// How long the clipping warning stays up after the mix last went over.
const CLIP_WARNING: Duration = Duration::from_millis(1500);
const MAX_LOG_ENTRIES: usize = 500;
/// The right-hand column's share of the window's width.
const SIDE_WIDTH: f32 = 0.45;
const RESOURCE_HEIGHT: f32 = 220.;
const LOG_HEIGHT: f32 = 200.;

const EXAMPLE: &str = r#"-- cmd-enter       run the selection, or the block under the cursor
-- cmd-shift-enter run everything
-- cmd-.           stop what the block plays into named slots
-- cmd-shift-.     stop all sound
-- cmd-r           start/stop recording to recordings/
-- cmd-/           comment/uncomment the selected lines

sample("kick.mp3").play

sample("kick.mp3").fit(500ms).play

sample("kick.mp3").fit(250ms).repeat(4).play

seq(
  sample("kick.mp3").fit(250ms),
  sample("kick.mp3").fit(125ms).repeat(2),
).repeat(2).play

(add(
  sample("kick.mp3") * -6db,
  sample("kick.mp3").fit(125ms).repeat(8),
) * limit).play

-- play starts right away, play(at 1bar) on the next bar (at 4b, at 5b + 2,
-- ...: any grid of beats); a named slot replaces what played there
120.bpm

notes("x . x . x x . .", 0.25b).play(sample("kick.mp3"), "drums", at 1bar)

-- a node's params are free until they're set (put the cursor on a name to
-- see them on the right); a pattern sets the note
let lead = wavetable:pos(0.3):warp(0.2) * duck("drums") * 0.5
notes("c3 e3 g3 _ b3 . g3 e3", 0.25b).play(lead, "lead", at 1bar)

-- effects are applied by multiplying; cutoffs are pitches: 800hz, c6, ?note + 24
((noise("pink") * bandpass(2khz, 0.6)).fit(1b) * echo(0.75b, 0.6) * spread * pan(-0.5)).play
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

/// The resource under the cursor, shown in the panel below the code.
struct OpenResource {
    /// With its range in the whole text.
    reference: ResourceRef,
    view: ResourceView,
    _subscription: Option<Subscription>,
}

enum ResourceView {
    Sample(Entity<SampleEditor>),
    Envelope(Entity<EnvelopeEditor>),
    Modulation(Entity<ModulationEditor>),
    /// A kind of resource that has no editor yet.
    Unsupported,
}

pub struct Workspace {
    path: PathBuf,
    /// The resources in the `.rock` file (shared with the session's evaluator).
    bundle: Bundle,
    /// Whether the code or the resources changed since the file was saved.
    dirty: bool,
    editor: Entity<EditorState>,
    flash: RangeDecorationCollection,
    comments: TextDecorationCollection,
    error: TextDecorationCollection,
    /// A frame around the resource reference under the cursor.
    resource_frame: RangeDecorationCollection,
    framed: Option<Range<usize>>,
    resource: Option<OpenResource>,
    overviews: OverviewCache,
    /// What the documentation panel shows: the last name under the cursor
    /// that had documentation (it stays while the cursor is elsewhere).
    doc: Option<Doc>,
    /// The name it was last looked up for, so it's only looked up again
    /// when that changes (or code runs, which can change what a name is).
    doc_lookup: Option<Lookup>,
    /// Bumped on every flash, so an old flash's timer doesn't clear a newer one.
    flash_generation: u64,
    session: Option<Session>,
    voices: usize,
    /// The session's count of frames that went over full scale, as last
    /// seen, and when it last went up: the mix is shown as clipping for a
    /// while after that.
    clipped: u64,
    clipped_at: Option<Instant>,
    clipping: bool,
    /// Whole seconds recorded, as last shown (so the timer redraws once a second).
    recorded_secs: Option<u64>,
    /// Tempo and position, as last shown: "120 bpm  3.2" (bar 3, beat 2).
    clock: String,
    log: Vec<LogEntry>,
    log_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// `code` is what `bundle::open` read from `path` (`None` for a new file),
    /// and the session was started with `bundle`.
    pub fn new(
        path: PathBuf,
        code: Result<Option<String>, String>,
        bundle: Bundle,
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

        let text = match code {
            Ok(Some(text)) => {
                info(LogKind::Info, format!("opened {}", path.display()));
                text
            }
            Ok(None) => {
                info(
                    LogKind::Info,
                    format!("new file {} (cmd-s to save)", path.display()),
                );
                EXAMPLE.to_string()
            }
            Err(e) => {
                info(LogKind::Error, format!("can't open {e}"));
                String::new()
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
        let (flash, comments, error, resource_frame) = editor.update(cx, |state, cx| {
            (
                state.create_range_decorations_collection(vec![], cx),
                state.create_decorations_collection(vec![], cx),
                state.create_decorations_collection(vec![], cx),
                state.create_range_decorations_collection(vec![], cx),
            )
        });

        let subscriptions = vec![
            cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.highlight_comments(cx);
                    if !this.dirty {
                        this.dirty = true;
                        cx.notify();
                    }
                }
            }),
            // There's no event for cursor moves, but the editor redraws on them.
            cx.observe_in(&editor, window, |this, _, window, cx| {
                this.update_resource(window, cx)
            }),
        ];

        // The audio thread can't call us, so poll its voice count for the status bar.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    let clock = this.session.as_ref().map_or_else(String::new, |s| {
                        let (bpm, beat) = s.position();
                        let beats = beat.floor() as u64;
                        format!("{bpm:.0} bpm  {}.{}", beats / 4 + 1, beats % 4 + 1)
                    });
                    if clock != this.clock {
                        this.clock = clock;
                        cx.notify();
                    }
                    let voices = this.session.as_ref().map_or(0, |s| s.voices());
                    let recorded = this
                        .session
                        .as_ref()
                        .and_then(|s| s.recording_time())
                        .map(|t| t.as_secs());
                    if voices != this.voices || recorded != this.recorded_secs {
                        this.voices = voices;
                        this.recorded_secs = recorded;
                        cx.notify();
                    }
                    let clipped = this.session.as_ref().map_or(0, |s| s.clipped());
                    if clipped > this.clipped {
                        this.clipped = clipped;
                        this.clipped_at = Some(Instant::now());
                    }
                    let clipping = this
                        .clipped_at
                        .is_some_and(|at| at.elapsed() < CLIP_WARNING);
                    if clipping != this.clipping {
                        this.clipping = clipping;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut workspace = Self {
            path,
            bundle,
            dirty: false,
            editor,
            flash,
            comments,
            error,
            resource_frame,
            framed: None,
            resource: None,
            overviews: OverviewCache::default(),
            doc: None,
            doc_lookup: None,
            flash_generation: 0,
            session,
            voices: 0,
            recorded_secs: None,
            clipped: 0,
            clipped_at: None,
            clipping: false,
            clock: String::new(),
            log,
            log_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        workspace.highlight_comments(cx);
        workspace.update_resource(window, cx);
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

    /// Whether the status bar warns that the mix is going over full scale.
    pub fn is_clipping(&self) -> bool {
        self.clipping
    }

    /// What the documentation panel shows.
    pub fn doc(&self) -> Option<&Doc> {
        self.doc.as_ref()
    }

    /// The resource reference under the cursor, if any.
    pub fn resource(&self) -> Option<&ResourceRef> {
        self.resource.as_ref().map(|open| &open.reference)
    }

    /// Whether the sample under the cursor has been loaded and drawn.
    pub fn sample_loaded(&self, cx: &App) -> bool {
        match self.resource.as_ref().map(|open| &open.view) {
            Some(ResourceView::Sample(editor)) => editor.read(cx).is_loaded(),
            _ => false,
        }
    }

    /// The resources in the `.rock` file.
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// Whether there are changes that haven't been saved.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The folder the `.rock` file is in.
    fn code_dir(&self) -> PathBuf {
        match self.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }

    /// The console contents, oldest first.
    pub fn log_lines(&self) -> impl Iterator<Item = &str> {
        self.log.iter().map(|entry| entry.text.as_str())
    }

    /// The whole text, and the selection or else the block under the cursor.
    fn current_block(&self, cx: &App) -> (String, Range<usize>) {
        let state = self.editor.read(cx);
        let text = state.value().to_string();
        let selection = state.selected_range();
        let range = if selection.is_empty() {
            blocks::block_at(&text, state.cursor())
        } else {
            selection
        };
        (text, range)
    }

    fn run_block(&mut self, _: &RunBlock, _: &mut Window, cx: &mut Context<Self>) {
        let (text, range) = self.current_block(cx);
        self.run(&text, range, cx);
    }

    /// Stop what the current block plays into named slots, without running it.
    fn stop_block(&mut self, _: &StopBlock, _: &mut Window, cx: &mut Context<Self>) {
        let (text, range) = self.current_block(cx);
        let code = &text[range.clone()];
        if code.trim().is_empty() {
            return;
        }
        self.flash(range.clone(), cx);
        let Some(session) = &mut self.session else {
            self.push_log(LogKind::Error, "no audio device".into(), cx);
            return;
        };
        match session.stop_named(code) {
            Ok(names) if names.is_empty() => self.push_log(
                LogKind::Info,
                "nothing to stop: this block plays into no named slot (cmd-shift-. stops everything)"
                    .into(),
                cx,
            ),
            Ok(names) => {
                self.set_error(None, cx);
                self.push_log(LogKind::Info, format!("stop {}", names.join(", ")), cx);
            }
            Err(e) => self.report_error(range.start + e.pos, &e.msg, cx),
        }
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

    fn toggle_recording(&mut self, _: &ToggleRecording, _: &mut Window, cx: &mut Context<Self>) {
        let dir = self.code_dir().join("recordings");
        let Some(session) = &mut self.session else {
            self.push_log(LogKind::Error, "no audio device".into(), cx);
            return;
        };
        if let Some((path, length)) = session.stop_recording() {
            let secs = length.as_secs();
            self.recorded_secs = None;
            self.push_log(
                LogKind::Info,
                format!("saved {} ({}:{:02})", path.display(), secs / 60, secs % 60),
                cx,
            );
            return;
        }
        let name = chrono::Local::now()
            .format("%Y-%m-%d %H.%M.%S.wav")
            .to_string();
        let path = dir.join(name);
        match session.start_recording(&path) {
            Ok(()) => {
                self.recorded_secs = Some(0);
                self.push_log(
                    LogKind::Info,
                    format!("recording to {}", path.display()),
                    cx,
                );
            }
            Err(e) => self.push_log(LogKind::Error, format!("can't record: {e}"), cx),
        }
    }

    /// Comment out the selected lines, or uncomment them if they all are.
    fn toggle_comment(&mut self, _: &ToggleComment, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            let toggle = comments::toggle(&state.value(), state.selected_range());
            // `replace` edits the selection, as one undo step.
            state.set_selected_range(toggle.range, cx);
            state.replace(toggle.text, window, cx);
            state.set_selected_range(toggle.selection, cx);
        });
    }

    fn save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value();
        match bundle::save(&self.path, text.as_str(), &self.bundle) {
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
            Err(e) => self.report_error(range.start + e.pos, &e.msg, cx),
        }
        // What the code defined may have changed what a name is.
        self.doc_lookup = None;
        self.update_doc(cx);
    }

    /// Show an error at byte offset `at`: squiggle and console line.
    fn report_error(&mut self, at: usize, msg: &str, cx: &mut Context<Self>) {
        let pos = self.editor.read(cx).text().offset_to_position(at);
        self.set_error(Some(at), cx);
        self.push_log(
            LogKind::Error,
            format!("{}:{}: {msg}", pos.line + 1, pos.character + 1),
            cx,
        );
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

    /// Follow the cursor: document the name it's on. A name without
    /// documentation (or no name) leaves the last one shown.
    fn update_doc(&mut self, cx: &mut Context<Self>) {
        let state = self.editor.read(cx);
        let lookup = docs::lookup_at(state.value().as_ref(), state.cursor());
        if lookup.is_none() || lookup == self.doc_lookup {
            return;
        }
        let doc = match &lookup {
            Some(Lookup::Param(name)) => Doc::of_param(name),
            Some(Lookup::Name(name)) => match &self.session {
                Some(session) => session.describe(name),
                None => Doc::builtin(name),
            },
            None => None,
        };
        self.doc_lookup = lookup;
        if doc.is_some() && doc != self.doc {
            self.doc = doc;
            cx.notify();
        }
    }

    /// Follow the cursor: frame the resource reference it's on, and show that
    /// resource's editor (or an empty panel if it's on none).
    fn update_resource(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_doc(cx);
        let state = self.editor.read(cx);
        let text = state.value().to_string();
        let cursor = state.cursor();
        let block = blocks::block_at(&text, cursor);
        let found =
            resource::reference_at(&text[block.clone()], cursor - block.start).map(|mut r| {
                r.range = r.range.start + block.start..r.range.end + block.start;
                r
            });

        // Only touch the decorations when they change: setting them redraws the
        // editor, which would bring us right back here.
        let range = found.as_ref().map(|r| r.range.clone());
        if range != self.framed {
            let color = cx.theme().muted_foreground.opacity(0.6);
            let frame = range.clone().map(|range| {
                RangeDecoration::new(range)
                    .with_style(RangeDecorationStyle::Frame)
                    .with_color(color)
            });
            self.resource_frame.set(frame.into_iter().collect(), cx);
            self.framed = range;
        }

        let Some(found) = found else {
            if self.resource.take().is_some() {
                cx.notify();
            }
            return;
        };
        if let Some(open) = &mut self.resource
            && open.reference.kind == found.kind
            && open.reference.name == found.name
        {
            if let ResourceView::Sample(editor) = &open.view {
                editor.update(cx, |editor, cx| editor.set_times(found.times.clone(), cx));
            }
            open.reference = found;
            return;
        }
        self.resource = Some(self.open_resource(found, window, cx));
        cx.notify();
    }

    fn open_resource(
        &mut self,
        reference: ResourceRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> OpenResource {
        let bundle = self.bundle.clone();
        let name = reference.name.clone();
        let (view, subscription) = match reference.kind {
            ResourceKind::Sample => {
                let cache = self.overviews.clone();
                let times = reference.times.clone();
                let editor = cx.new(|cx| SampleEditor::new(&name, times, bundle, cache, cx));
                let subscription = self.log_resource_events(&editor, cx);
                (ResourceView::Sample(editor), Some(subscription))
            }
            ResourceKind::Envelope => {
                let editor = cx.new(|_| EnvelopeEditor::new(&name, bundle));
                let subscription = self.log_resource_events(&editor, cx);
                (ResourceView::Envelope(editor), Some(subscription))
            }
            ResourceKind::Modulation => {
                let editor = cx.new(|cx| ModulationEditor::new(&name, bundle, window, cx));
                let subscription = self.log_resource_events(&editor, cx);
                (ResourceView::Modulation(editor), Some(subscription))
            }
            ResourceKind::Wavetable | ResourceKind::Midi => (ResourceView::Unsupported, None),
        };
        OpenResource {
            reference,
            view,
            _subscription: subscription,
        }
    }

    fn log_resource_events<E: EventEmitter<ResourceEvent>>(
        &mut self,
        editor: &Entity<E>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(editor, |this, _, event: &ResourceEvent, cx| {
            let (kind, text) = match event {
                ResourceEvent::Info(text) => (LogKind::Info, text.clone()),
                ResourceEvent::Error(text) => (LogKind::Error, text.clone()),
                ResourceEvent::Changed => {
                    if !this.dirty {
                        this.dirty = true;
                        cx.notify();
                    }
                    return;
                }
            };
            this.push_log(kind, text, cx);
        })
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
            .when(self.clipping, |this| {
                // Not clipped, in fact: the limiter caught it, but it's
                // squashing the mix.
                this.child(
                    div()
                        .text_color(theme.danger)
                        .child("clipping: limited, turn it down"),
                )
            })
            .child(self.clock.clone())
            .child(
                div()
                    .id("record")
                    .cursor_pointer()
                    .hover(|this| this.text_color(theme.foreground))
                    .map(|this| match self.recorded_secs {
                        Some(secs) => this.text_color(theme.danger).child(format!(
                            "● {}:{:02}",
                            secs / 60,
                            secs % 60
                        )),
                        None => this.child("● rec"),
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_recording(&ToggleRecording, window, cx)
                    })),
            )
            .child(
                div()
                    .when(self.voices > 0, |this| this.text_color(theme.success))
                    .child(voices),
            )
    }

    /// A panel in the right-hand column, with a heading.
    fn panel(title: &'static str, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        v_flex().border_t_1().border_color(theme.border).child(
            div()
                .flex_shrink_0()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(title),
        )
    }

    fn render_doc(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, mono) = (theme.muted_foreground, theme.mono_font_family.clone());
        let body = match &self.doc {
            None => div()
                .text_sm()
                .text_color(muted)
                .child(
                    "Put the cursor on a name (wavetable, lowpass, notes, a let) or a \
                     param (:note) to see what it is and what params it has.",
                )
                .into_any_element(),
            Some(doc) => v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .child(doc.title.clone()),
                        )
                        .child(div().text_xs().text_color(muted).child(doc.kind.clone())),
                )
                .child(div().text_sm().child(doc.summary.clone()))
                .when(!doc.params.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .gap_0p5()
                            .text_sm()
                            .child(div().text_xs().text_color(muted).child("Params"))
                            .children(doc.params.iter().map(|p| {
                                h_flex()
                                    .gap_2()
                                    .items_start()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_family(mono.clone())
                                            .child(p.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_family(mono.clone())
                                            .text_color(muted)
                                            .child(p.default.clone()),
                                    )
                                    .child(div().text_color(muted).child(p.doc.clone()))
                            })),
                    )
                })
                .when(!doc.examples.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_xs().text_color(muted).child("Examples"))
                            .children(doc.examples.iter().map(|e| {
                                div().font_family(mono.clone()).text_sm().child(e.clone())
                            })),
                    )
                })
                .into_any_element(),
        };
        Self::panel("DOCUMENTATION", cx).flex_1().min_h_0().child(
            div()
                .id("doc")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_3()
                .pb_2()
                .child(body),
        )
    }

    fn render_resource(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let content = match self
            .resource
            .as_ref()
            .map(|open| (&open.view, &open.reference))
        {
            Some((ResourceView::Sample(editor), _)) => editor.clone().into_any_element(),
            Some((ResourceView::Envelope(editor), _)) => editor.clone().into_any_element(),
            Some((ResourceView::Modulation(editor), _)) => editor.clone().into_any_element(),
            Some((ResourceView::Unsupported, reference)) => {
                let kind = reference.kind.function();
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!(
                        "{kind} \"{}\": there's no {kind} editor yet",
                        reference.name
                    ))
                    .into_any_element()
            }
            None => div()
                .text_sm()
                .text_color(muted)
                .child("Put the cursor on a sample, envelope or mod to edit it here.")
                .into_any_element(),
        };
        Self::panel("EDITOR", cx)
            .h(px(RESOURCE_HEIGHT))
            .flex_shrink_0()
            .child(div().flex_1().min_h_0().px_3().pb_2().child(content))
    }

    fn render_log(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let entries = div()
            .id("log")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.log_scroll)
            .px_3()
            .pb_2()
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
            }));
        Self::panel("LOGS", cx)
            .h(px(LOG_HEIGHT))
            .flex_shrink_0()
            .child(entries)
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let doc = self.render_doc(cx);
        let resource = self.render_resource(cx);
        let log = self.render_log(cx);
        let status_bar = self.render_status_bar(cx);
        let theme = cx.theme();
        v_flex()
            .key_context("Workspace")
            .on_action(cx.listener(Self::run_block))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::stop_block))
            .on_action(cx.listener(Self::stop_all))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::toggle_recording))
            .on_action(cx.listener(Self::toggle_comment))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(theme.mono_font_size)
                            .child(Editor::new(&self.editor).appearance(false).h_full()),
                    )
                    .child(
                        v_flex()
                            .w(relative(SIDE_WIDTH))
                            .flex_shrink_0()
                            .h_full()
                            .border_l_1()
                            .border_color(theme.border)
                            .bg(theme.secondary)
                            // The top panel's border would double the window's edge.
                            .child(doc.border_t_0())
                            .child(resource)
                            .child(log),
                    ),
            )
            .child(status_bar)
    }
}
