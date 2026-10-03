//! The editor for `envelope("...")`, like Ableton's ADSR editor.
//!
//! It's really two timelines drawn side by side: attack and decay count from
//! note-on, release counts from note-off. In between, the sustain level holds
//! for however long the note lasts, which isn't known here, so it's drawn at a
//! fixed width (shaded).
//!
//! Drag the squares to change the times (the decay square also sets the
//! sustain level, as does dragging the sustain line). Drag a diamond, or
//! alt-drag anywhere over a stage, to bend it; double-click a diamond to
//! straighten it.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rocktober_engine::envelope::{Envelope, Stage};
use rocktober_engine::resource::ResourceKind;

use crate::ResourceEvent;
use crate::curve_view::{self, GRAB, dragged_curve, format_seconds, inset, near, round, xy};

/// The share of the width given to the sustain.
const SUSTAIN_WIDTH: f32 = 0.2;

enum State {
    Missing,
    Loaded(Envelope),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum StageId {
    Attack,
    Decay,
    Release,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Handle {
    /// The peak: sets the attack time.
    Attack,
    /// The end of the decay: sets the decay time and the sustain level.
    Decay,
    /// The end of the release: sets the release time.
    Release,
    /// The sustain line: sets the sustain level.
    Sustain,
    Curve(StageId),
}

#[derive(Clone, Copy)]
struct Drag {
    handle: Handle,
    start_y: f32,
    start_curve: f64,
    /// The time scale is kept while dragging, so the view doesn't shift under
    /// the mouse; it's refitted afterwards.
    seconds: f64,
}

pub struct EnvelopeEditor {
    name: String,
    path: PathBuf,
    state: State,
    drag: Option<Drag>,
    hover: Option<Handle>,
    /// Show curves rather than times in the value row, like Ableton's
    /// Time/Slope switch.
    show_curves: bool,
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl EventEmitter<ResourceEvent> for EnvelopeEditor {}

/// How many seconds the width shows (minus the sustain): all stages, with room
/// to drag them longer.
fn visible_seconds(env: &Envelope) -> f64 {
    ((env.attack.time + env.decay.time + env.release.time) * 1.25).max(0.05)
}

/// A stage on screen: which one, where it starts and ends, and its curve.
type StageLine = (StageId, (f32, f32), (f32, f32), f64);

/// Where everything is drawn.
struct Layout {
    top: f32,
    height: f32,
    pixels_per_second: f32,
    /// Note-on, the peak, the end of the decay, the end of the sustain, the
    /// end of the release.
    start: f32,
    peak: f32,
    decayed: f32,
    released: f32,
    release_end: f32,
}

impl Layout {
    fn new(env: &Envelope, bounds: Bounds<Pixels>, seconds: f64) -> Self {
        let area = inset(bounds);
        let (start, top) = xy(area.origin);
        let width = area.size.width.as_f32();
        let pixels_per_second = width * (1.0 - SUSTAIN_WIDTH) / seconds as f32;
        let peak = start + env.attack.time as f32 * pixels_per_second;
        let decayed = peak + env.decay.time as f32 * pixels_per_second;
        let released = decayed + width * SUSTAIN_WIDTH;
        Self {
            top,
            height: area.size.height.as_f32(),
            pixels_per_second,
            start,
            peak,
            decayed,
            released,
            release_end: released + env.release.time as f32 * pixels_per_second,
        }
    }

    fn y(&self, level: f64) -> f32 {
        self.top + (1.0 - level as f32) * self.height
    }

    fn level(&self, y: f32) -> f64 {
        (1.0 - (y - self.top) / self.height).clamp(0.0, 1.0) as f64
    }

    fn bottom(&self) -> f32 {
        self.top + self.height
    }

    /// Each stage's start and end on screen, and its curve.
    fn stages(&self, env: &Envelope) -> [StageLine; 3] {
        let s = self.y(env.sustain);
        [
            (
                StageId::Attack,
                (self.start, self.y(0.0)),
                (self.peak, self.y(1.0)),
                env.attack.curve,
            ),
            (
                StageId::Decay,
                (self.peak, self.y(1.0)),
                (self.decayed, s),
                env.decay.curve,
            ),
            (
                StageId::Release,
                (self.released, s),
                (self.release_end, self.y(0.0)),
                env.release.curve,
            ),
        ]
    }

    /// The handle at a position, if any.
    fn handle_at(&self, env: &Envelope, pos: (f32, f32)) -> Option<Handle> {
        let squares = [
            (Handle::Release, (self.release_end, self.y(0.0))),
            (Handle::Decay, (self.decayed, self.y(env.sustain))),
            (Handle::Attack, (self.peak, self.y(1.0))),
        ];
        if let Some((handle, _)) = squares.iter().find(|(_, at)| near(*at, pos)) {
            return Some(*handle);
        }
        for (stage, from, to, curve) in self.stages(env) {
            if from.1 != to.1 && near(curve_view::segment_middle(from, to, curve), pos) {
                return Some(Handle::Curve(stage));
            }
        }
        let on_sustain = pos.0 > self.decayed
            && pos.0 < self.released
            && (pos.1 - self.y(env.sustain)).abs() <= GRAB;
        on_sustain.then_some(Handle::Sustain)
    }

    /// The stage whose stretch of time a position is over.
    fn stage_at(&self, x: f32) -> Option<StageId> {
        if x >= self.start && x < self.peak {
            Some(StageId::Attack)
        } else if x >= self.peak && x < self.decayed {
            Some(StageId::Decay)
        } else if x >= self.released && x <= self.release_end {
            Some(StageId::Release)
        } else {
            None
        }
    }
}

fn stage_mut(env: &mut Envelope, stage: StageId) -> &mut Stage {
    match stage {
        StageId::Attack => &mut env.attack,
        StageId::Decay => &mut env.decay,
        StageId::Release => &mut env.release,
    }
}

/// The levels a stage goes from and to.
fn stage_levels(env: &Envelope, stage: StageId) -> (f64, f64) {
    match stage {
        StageId::Attack => (0.0, 1.0),
        StageId::Decay => (1.0, env.sustain),
        StageId::Release => (env.sustain, 0.0),
    }
}

fn format_level(level: f64) -> String {
    if level < 1e-5 {
        "-inf dB".to_string()
    } else {
        format!("{:.1} dB", 20.0 * level.log10())
    }
}

impl EnvelopeEditor {
    /// `root` is the folder of the code file.
    pub fn new(name: &str, root: &Path) -> Self {
        let path = ResourceKind::Envelope.path(root, name);
        let state = if path.exists() {
            match Envelope::load(&path) {
                Ok(env) => State::Loaded(env),
                Err(e) => State::Failed(e),
            }
        } else {
            State::Missing
        };
        Self {
            name: name.to_string(),
            path,
            state,
            drag: None,
            hover: None,
            show_curves: false,
            bounds: Rc::new(Cell::new(Bounds::default())),
        }
    }

    pub fn envelope(&self) -> Option<&Envelope> {
        match &self.state {
            State::Loaded(env) => Some(env),
            _ => None,
        }
    }

    pub fn create(&mut self, cx: &mut Context<Self>) {
        self.state = State::Loaded(Envelope::default());
        if self.save(cx) {
            cx.emit(ResourceEvent::Info(format!(
                "created {}/{}",
                ResourceKind::Envelope.dir(),
                ResourceKind::Envelope.file_name(&self.name)
            )));
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let State::Loaded(env) = &self.state else {
            return false;
        };
        match env.save(&self.path) {
            Ok(()) => true,
            Err(e) => {
                cx.emit(ResourceEvent::Error(e));
                false
            }
        }
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let bounds = self.bounds.get();
        if e.button != MouseButton::Left || !bounds.contains(&e.position) {
            return;
        }
        let State::Loaded(env) = &mut self.state else {
            return;
        };
        let seconds = visible_seconds(env);
        let layout = Layout::new(env, bounds, seconds);
        let pos = xy(e.position);
        let handle = match layout.handle_at(env, pos) {
            Some(Handle::Curve(stage)) if e.click_count == 2 => {
                stage_mut(env, stage).curve = 0.0;
                self.save(cx);
                cx.notify();
                return;
            }
            None if e.modifiers.alt => layout.stage_at(pos.0).map(Handle::Curve),
            handle => handle,
        };
        let Some(handle) = handle else {
            return;
        };
        let start_curve = match handle {
            Handle::Curve(stage) => stage_mut(env, stage).curve,
            _ => 0.0,
        };
        self.drag = Some(Drag {
            handle,
            start_y: pos.1,
            start_curve,
            seconds,
        });
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let bounds = self.bounds.get();
        let State::Loaded(env) = &mut self.state else {
            return;
        };
        let (x, y) = xy(e.position);
        let Some(drag) = self.drag else {
            let layout = Layout::new(env, bounds, visible_seconds(env));
            let hover = bounds
                .contains(&e.position)
                .then(|| layout.handle_at(env, (x, y)))
                .flatten();
            if hover != self.hover {
                self.hover = hover;
                cx.notify();
            }
            return;
        };

        let layout = Layout::new(env, bounds, drag.seconds);
        let time = |from: f32| {
            let seconds = (x - from) / layout.pixels_per_second;
            round(seconds.clamp(0.0, Envelope::MAX_TIME as f32) as f64, 4)
        };
        let level = |y: f32| round(layout.level(y), 3);
        match drag.handle {
            Handle::Attack => env.attack.time = time(layout.start),
            Handle::Decay => {
                env.decay.time = time(layout.peak);
                env.sustain = level(y);
            }
            Handle::Release => env.release.time = time(layout.released),
            Handle::Sustain => env.sustain = level(y),
            Handle::Curve(stage) => {
                let (from, to) = stage_levels(env, stage);
                stage_mut(env, stage).curve = round(
                    dragged_curve(drag.start_curve, drag.start_y - y, from, to),
                    2,
                );
            }
        }
        cx.notify();
    }

    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.save(cx);
            cx.notify();
        }
    }

    fn render_envelope(&self, env: &Envelope, cx: &mut Context<Self>) -> AnyElement {
        let held = cx.theme().secondary;
        let env = env.clone();
        let active = self.drag.map(|d| d.handle).or(self.hover);
        let seconds = self
            .drag
            .map_or_else(|| visible_seconds(&env), |d| d.seconds);
        let bounds_cell = self.bounds.clone();
        let this = cx.entity().downgrade();

        canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, (), window, _| {
                let layout = Layout::new(&env, bounds, seconds);
                window.paint_quad(fill(
                    Bounds::from_corners(
                        point(px(layout.decayed), px(layout.top)),
                        point(px(layout.released), px(layout.bottom())),
                    ),
                    held,
                ));

                let stages = layout.stages(&env);
                let mut line = vec![stages[0].1];
                for (_, from, to, curve) in stages {
                    if line.last() != Some(&from) {
                        line.push(from);
                    }
                    curve_view::segment_points(from, to, curve, &mut line);
                }
                curve_view::paint_line(&line, layout.bottom(), window);

                for (stage, from, to, curve) in stages {
                    if from.1 != to.1 {
                        let middle = curve_view::segment_middle(from, to, curve);
                        let on = active == Some(Handle::Curve(stage));
                        curve_view::paint_curve_handle(middle, on, window);
                    }
                }
                let squares = [
                    (Handle::Attack, (layout.peak, layout.y(1.0))),
                    (Handle::Decay, (layout.decayed, layout.y(env.sustain))),
                    (Handle::Release, (layout.release_end, layout.y(0.0))),
                ];
                for (handle, at) in squares {
                    let on = active == Some(handle)
                        || (handle == Handle::Decay && active == Some(Handle::Sustain));
                    curve_view::paint_point(at, on, window);
                }

                let weak = this.clone();
                window.on_mouse_event(move |e: &MouseDownEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = weak.update(cx, |this, cx| this.mouse_down(e, cx));
                    }
                });
                let weak = this.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = weak.update(cx, |this, cx| this.mouse_move(e, cx));
                    }
                });
                let weak = this.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = weak.update(cx, |this, cx| this.mouse_up(cx));
                    }
                });
            },
        )
        .size_full()
        .into_any_element()
    }

    /// The row under the envelope: A D S R values, as times or curves.
    fn render_values(&self, env: &Envelope, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, foreground) = (theme.muted_foreground, theme.foreground);
        let value = |label: &'static str, text: String| {
            h_flex()
                .gap_1()
                .child(div().text_color(muted).child(label))
                .child(div().text_color(foreground).child(text))
        };
        let (a, d, r) = if self.show_curves {
            let curve = |s: Stage| format!("{:+.2}", s.curve);
            (curve(env.attack), curve(env.decay), curve(env.release))
        } else {
            let time = |s: Stage| format_seconds(s.time);
            (time(env.attack), time(env.decay), time(env.release))
        };
        let tab = |id: &'static str, label: &'static str, selected: bool| {
            div()
                .id(id)
                .cursor_pointer()
                .text_color(if selected { foreground } else { muted })
                .when(selected, |this| this.underline())
                .child(label)
        };
        h_flex()
            .gap_4()
            .text_xs()
            .child(
                tab("times", "Time", !self.show_curves).on_click(cx.listener(|this, _, _, cx| {
                    this.show_curves = false;
                    cx.notify();
                })),
            )
            .child(
                tab("curves", "Curve", self.show_curves).on_click(cx.listener(|this, _, _, cx| {
                    this.show_curves = true;
                    cx.notify();
                })),
            )
            .child(div().w(px(8.)))
            .child(value("A", a))
            .child(value("D", d))
            .child(value("S", format_level(env.sustain)))
            .child(value("R", r))
    }
}

impl Render for EnvelopeEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (foreground, danger) = (theme.foreground, theme.danger);
        let header = div()
            .text_xs()
            .text_color(foreground)
            .child(format!("envelope \"{}\"", self.name));

        let body = match &self.state {
            State::Loaded(env) => {
                let env = env.clone();
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .child(
                        div()
                            .id("envelope-curve")
                            .flex_1()
                            .min_h_0()
                            .child(self.render_envelope(&env, cx))
                            .test_support(),
                    )
                    .child(self.render_values(&env, cx))
                    .into_any_element()
            }
            State::Failed(e) => div()
                .flex_1()
                .text_sm()
                .text_color(danger)
                .child(e.clone())
                .into_any_element(),
            State::Missing => {
                let file = format!(
                    "{}/{}",
                    ResourceKind::Envelope.dir(),
                    ResourceKind::Envelope.file_name(&self.name)
                );
                let this = cx.entity().downgrade();
                curve_view::missing(
                    file,
                    move |_, _, cx| {
                        let _ = this.update(cx, |this, cx| this.create(cx));
                    },
                    cx,
                )
            }
        };

        v_flex().size_full().gap_2().child(header).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{Envelope, Handle, Layout, StageId, format_level};
    use gpui_kit::{Bounds, point, px};

    #[test]
    fn handles_follow_the_envelope() {
        let env = Envelope::default();
        let bounds = Bounds::from_corners(point(px(0.), px(0.)), point(px(116.), px(116.)));
        let l = Layout::new(&env, bounds, 1.0);
        assert_eq!(l.start, 8.0);
        assert!((l.peak - (8.0 + 0.001 * l.pixels_per_second)).abs() < 1e-4);
        assert_eq!(
            l.handle_at(&env, (l.decayed, l.y(0.5))),
            Some(Handle::Decay)
        );
        assert_eq!(
            l.handle_at(&env, (l.release_end, l.y(0.0))),
            Some(Handle::Release)
        );
        let middle = (l.decayed + l.released) / 2.0;
        assert_eq!(
            l.handle_at(&env, (middle, l.y(0.5) + 3.0)),
            Some(Handle::Sustain)
        );
        assert_eq!(l.handle_at(&env, (middle, l.y(0.9))), None);
        assert_eq!(l.stage_at(l.released + 1.0), Some(StageId::Release));
    }

    #[test]
    fn levels() {
        assert_eq!(format_level(0.5), "-6.0 dB");
        assert_eq!(format_level(1.0), "0.0 dB");
        assert_eq!(format_level(0.0), "-inf dB");
    }
}
