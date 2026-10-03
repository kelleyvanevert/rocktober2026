//! The editor for `modulation("...")`: one value (0..1) over a fixed length,
//! drawn as points joined by curved segments, like an Ableton clip envelope.
//!
//! Click to add a point (and keep dragging to place it), drag a point to move
//! it, alt-click a point to remove it, alt-drag a segment to bend it.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Sizable, h_flex, v_flex};
use gpui_kit::*;
use rocktober_engine::lang::{self, Expr};
use rocktober_engine::modulation::{self, Modulation};
use rocktober_engine::resource::ResourceKind;

use crate::ResourceEvent;
use crate::curve_view::{self, dragged_curve, inset, near, round, xy};
use crate::sample_editor::format_time;

enum State {
    Missing,
    Loaded(Modulation),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Point(usize),
    /// Bending the segment that starts at this point.
    Curve {
        segment: usize,
        start_y: f32,
        start_curve: f64,
    },
}

pub struct ModulationEditor {
    name: String,
    path: PathBuf,
    state: State,
    drag: Option<Drag>,
    /// The point under the mouse.
    hover: Option<usize>,
    /// Where the curve was last drawn, to turn mouse positions into values.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    length: Entity<InputState>,
    _subscription: Subscription,
}

impl EventEmitter<ResourceEvent> for ModulationEditor {}

/// A length the way it would be written in code: `2s`, `1.25s`, `500ms`.
pub fn format_length(seconds: f64) -> String {
    let ms = (seconds * 1000.0).round();
    if ms < 1000.0 {
        format!("{ms}ms")
    } else {
        format!("{}s", ms / 1000.0)
    }
}

/// A duration typed by the user, in the language's syntax.
fn parse_length(text: &str) -> Option<f64> {
    match lang::parse(text).ok()?.as_slice() {
        [e] => match e.expr {
            Expr::Duration(seconds) if seconds >= Modulation::MIN_LENGTH => Some(seconds),
            _ => None,
        },
        _ => None,
    }
}

/// Where a modulation point is drawn.
fn to_screen(area: Bounds<Pixels>, p: &modulation::Point) -> (f32, f32) {
    let (left, top) = xy(area.origin);
    let (w, h) = (area.size.width.as_f32(), area.size.height.as_f32());
    (left + p.at as f32 * w, top + (1.0 - p.value as f32) * h)
}

/// The position and value at a point on the screen.
fn from_screen(area: Bounds<Pixels>, (x, y): (f32, f32)) -> (f64, f64) {
    let (left, top) = xy(area.origin);
    let (w, h) = (area.size.width.as_f32(), area.size.height.as_f32());
    let at = ((x - left) / w).clamp(0.0, 1.0);
    let value = (1.0 - (y - top) / h).clamp(0.0, 1.0);
    (round(at as f64, 4), round(value as f64, 3))
}

impl ModulationEditor {
    /// `root` is the folder of the code file.
    pub fn new(name: &str, root: &Path, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = ResourceKind::Modulation.path(root, name);
        let state = if path.exists() {
            match Modulation::load(&path) {
                Ok(m) => State::Loaded(m),
                Err(e) => State::Failed(e),
            }
        } else {
            State::Missing
        };
        let length = cx.new(|cx| InputState::new(window, cx));
        let subscription = cx.subscribe_in(
            &length,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } | InputEvent::Blur = event {
                    this.apply_length(window, cx);
                }
            },
        );
        let mut editor = Self {
            name: name.to_string(),
            path,
            state,
            drag: None,
            hover: None,
            bounds: Rc::new(Cell::new(Bounds::default())),
            length,
            _subscription: subscription,
        };
        editor.show_length(window, cx);
        editor
    }

    pub fn modulation(&self) -> Option<&Modulation> {
        match &self.state {
            State::Loaded(m) => Some(m),
            _ => None,
        }
    }

    pub fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = State::Loaded(Modulation::default());
        if self.save(cx) {
            cx.emit(ResourceEvent::Info(format!(
                "created {}/{}",
                ResourceKind::Modulation.dir(),
                ResourceKind::Modulation.file_name(&self.name)
            )));
        }
        self.show_length(window, cx);
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let State::Loaded(m) = &self.state else {
            return false;
        };
        match m.save(&self.path) {
            Ok(()) => true,
            Err(e) => {
                cx.emit(ResourceEvent::Error(e));
                false
            }
        }
    }

    fn show_length(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let State::Loaded(m) = &self.state {
            let text = format_length(m.length);
            self.length
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
    }

    fn apply_length(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let State::Loaded(m) = &mut self.state else {
            return;
        };
        let text = self.length.read(cx).value().to_string();
        match parse_length(&text) {
            Some(seconds) if seconds != m.length => {
                m.length = seconds;
                self.save(cx);
                cx.notify();
            }
            Some(_) => {}
            None => cx.emit(ResourceEvent::Error(format!(
                "length: expected a duration like 2s or 500ms, got \"{}\"",
                text.trim()
            ))),
        }
        self.show_length(window, cx);
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let bounds = self.bounds.get();
        if e.button != MouseButton::Left || !bounds.contains(&e.position) {
            return;
        }
        let State::Loaded(m) = &mut self.state else {
            return;
        };
        let area = inset(bounds);
        let pos = xy(e.position);
        let hit = m.points.iter().position(|p| near(to_screen(area, p), pos));
        let mut changed = false;
        match (hit, e.modifiers.alt) {
            (Some(i), true) => {
                if m.points.len() > 1 {
                    m.points.remove(i);
                    self.hover = None;
                    changed = true;
                }
            }
            (Some(i), false) => self.drag = Some(Drag::Point(i)),
            (None, true) => {
                let (at, _) = from_screen(area, pos);
                let next = m.points.partition_point(|p| p.at <= at);
                if next > 0 && next < m.points.len() {
                    self.drag = Some(Drag::Curve {
                        segment: next - 1,
                        start_y: pos.1,
                        start_curve: m.points[next - 1].curve,
                    });
                }
            }
            (None, false) => {
                let (at, value) = from_screen(area, pos);
                let i = m.points.partition_point(|p| p.at <= at);
                // Splitting a segment: both halves keep its curve.
                let curve = if i > 0 { m.points[i - 1].curve } else { 0.0 };
                m.points.insert(i, modulation::Point { at, value, curve });
                self.drag = Some(Drag::Point(i));
                self.hover = Some(i);
                changed = true;
            }
        }
        if changed {
            self.save(cx);
        }
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let State::Loaded(m) = &mut self.state else {
            return;
        };
        let area = inset(self.bounds.get());
        let pos = xy(e.position);
        match self.drag {
            Some(Drag::Point(i)) => {
                let (at, value) = from_screen(area, pos);
                let lo = if i > 0 { m.points[i - 1].at } else { 0.0 };
                let hi = m.points.get(i + 1).map_or(1.0, |p| p.at);
                m.points[i].at = at.clamp(lo, hi);
                m.points[i].value = value;
                cx.notify();
            }
            Some(Drag::Curve {
                segment,
                start_y,
                start_curve,
            }) => {
                let (from, to) = (m.points[segment].value, m.points[segment + 1].value);
                m.points[segment].curve =
                    round(dragged_curve(start_curve, start_y - pos.1, from, to), 2);
                cx.notify();
            }
            None => {
                let hover = self
                    .bounds
                    .get()
                    .contains(&e.position)
                    .then(|| m.points.iter().position(|p| near(to_screen(area, p), pos)))
                    .flatten();
                if hover != self.hover {
                    self.hover = hover;
                    cx.notify();
                }
            }
        }
    }

    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.save(cx);
            cx.notify();
        }
    }

    /// What the mouse is on, for the header.
    fn status(&self, m: &Modulation) -> String {
        match self.drag {
            Some(Drag::Curve { segment, .. }) => {
                format!("curve {:.2}", m.points[segment].curve)
            }
            Some(Drag::Point(i)) => self.describe_point(m, i),
            None => match self.hover {
                Some(i) => self.describe_point(m, i),
                None => String::new(),
            },
        }
    }

    fn describe_point(&self, m: &Modulation, i: usize) -> String {
        let p = m.points[i];
        format!("{}  →  {:.3}", format_time(p.at * m.length), p.value)
    }

    fn render_curve(&self, m: &Modulation, cx: &mut Context<Self>) -> AnyElement {
        let grid = cx.theme().border;
        let modulation = m.clone();
        let hover = self.hover;
        let drag = self.drag;
        let bounds_cell = self.bounds.clone();
        let this = cx.entity().downgrade();

        canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, (), window, _| {
                let area = inset(bounds);
                let (left, top) = xy(area.origin);
                let (right, bottom) = (area.right().as_f32(), area.bottom().as_f32());
                for i in 0..=4 {
                    let x = left + (right - left) * i as f32 / 4.0;
                    window.paint_quad(fill(
                        Bounds::from_corners(point(px(x), px(top)), point(px(x + 1.0), px(bottom))),
                        grid,
                    ));
                }

                let points: Vec<(f32, f32)> = modulation
                    .points
                    .iter()
                    .map(|p| to_screen(area, p))
                    .collect();
                let mut line = vec![(left, points[0].1), points[0]];
                for (i, pair) in points.windows(2).enumerate() {
                    curve_view::segment_points(
                        pair[0],
                        pair[1],
                        modulation.points[i].curve,
                        &mut line,
                    );
                }
                line.push((right, points[points.len() - 1].1));
                curve_view::paint_line(&line, bottom, window);

                for (i, pair) in points.windows(2).enumerate() {
                    if pair[0].1 != pair[1].1 {
                        let middle = curve_view::segment_middle(
                            pair[0],
                            pair[1],
                            modulation.points[i].curve,
                        );
                        let active =
                            matches!(drag, Some(Drag::Curve { segment, .. }) if segment == i);
                        curve_view::paint_curve_handle(middle, active, window);
                    }
                }
                for (i, &p) in points.iter().enumerate() {
                    let active = hover == Some(i) || drag == Some(Drag::Point(i));
                    curve_view::paint_point(p, active, window);
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
}

impl Render for ModulationEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, foreground, danger) = (theme.muted_foreground, theme.foreground, theme.danger);
        let mut header = h_flex().gap_3().text_xs().text_color(muted).child(
            div()
                .text_color(foreground)
                .child(format!("modulation \"{}\"", self.name)),
        );

        let body = match &self.state {
            State::Loaded(m) => {
                header = header
                    .child(
                        h_flex()
                            .gap_1()
                            .child("length")
                            .child(Input::new(&self.length).xsmall().w(px(80.))),
                    )
                    .child(div().flex_1())
                    .child(self.status(m));
                let m = m.clone();
                div()
                    .id("modulation-curve")
                    .flex_1()
                    .min_h_0()
                    .child(self.render_curve(&m, cx))
                    .test_support()
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
                    ResourceKind::Modulation.dir(),
                    ResourceKind::Modulation.file_name(&self.name)
                );
                let this = cx.entity().downgrade();
                curve_view::missing(
                    file,
                    move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| this.create(window, cx));
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
    use super::{format_length, parse_length};

    #[test]
    fn lengths_are_written_like_code() {
        assert_eq!(format_length(2.0), "2s");
        assert_eq!(format_length(1.25), "1.25s");
        assert_eq!(format_length(0.5), "500ms");
        assert_eq!(parse_length("1.25s"), Some(1.25));
        assert_eq!(parse_length(" 500ms "), Some(0.5));
        assert_eq!(parse_length("0:02"), Some(2.0));
        assert_eq!(parse_length("2"), None);
        assert_eq!(parse_length("0ms"), None);
    }
}
