//! Drawing and editing helpers shared by the envelope and modulation editors:
//! curved segments, handles, and the "doesn't exist yet" view.

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::Button;
use gpui_kit::*;
use rocktober_engine::curve;

/// Room around the drawing, so handles at the edges can still be grabbed.
pub const INSET: f32 = 8.0;
/// How close (in pixels) the mouse must be to grab a handle.
pub const GRAB: f32 = 7.0;
/// Pixels of vertical drag to bend a curve from straight to its extreme.
const CURVE_DRAG: f32 = 100.0;

/// Ableton's colors: a cyan line, orange points, pink curve handles.
pub fn line_color() -> Hsla {
    hsla(190. / 360., 0.65, 0.6, 1.)
}
pub fn point_color() -> Hsla {
    hsla(30. / 360., 0.9, 0.55, 1.)
}
pub fn curve_color() -> Hsla {
    hsla(350. / 360., 0.65, 0.65, 1.)
}

pub fn inset(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::from_corners(
        point(bounds.left() + px(INSET), bounds.top() + px(INSET)),
        point(bounds.right() - px(INSET), bounds.bottom() - px(INSET)),
    )
}

/// Points along a curved segment, excluding its start.
pub fn segment_points(from: (f32, f32), to: (f32, f32), bend: f64, out: &mut Vec<(f32, f32)>) {
    const STEPS: usize = 32;
    for i in 1..=STEPS {
        let u = i as f64 / STEPS as f64;
        let y = curve::segment(from.1 as f64, to.1 as f64, u, bend) as f32;
        out.push((from.0 + (to.0 - from.0) * u as f32, y));
    }
}

/// The middle of a curved segment, where its curve handle sits.
pub fn segment_middle(from: (f32, f32), to: (f32, f32), bend: f64) -> (f32, f32) {
    let y = curve::segment(from.1 as f64, to.1 as f64, 0.5, bend) as f32;
    ((from.0 + to.0) / 2.0, y)
}

/// Stroke a polyline, and fill the area between it and `floor` faintly.
pub fn paint_line(points: &[(f32, f32)], floor: f32, window: &mut Window) {
    let to_point = |&(x, y): &(f32, f32)| point(px(x), px(y));
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return;
    };
    let mut area: Vec<Point<Pixels>> = points.iter().map(to_point).collect();
    area.push(point(px(last.0), px(floor)));
    area.push(point(px(first.0), px(floor)));
    let mut fill = PathBuilder::fill();
    fill.add_polygon(&area, true);
    if let Ok(path) = fill.build() {
        window.paint_path(path, line_color().opacity(0.12));
    }

    let line: Vec<Point<Pixels>> = points.iter().map(to_point).collect();
    let mut stroke = PathBuilder::stroke(px(1.5));
    stroke.add_polygon(&line, false);
    if let Ok(path) = stroke.build() {
        window.paint_path(path, line_color());
    }
}

/// An outlined square handle, for points.
pub fn paint_point(at: (f32, f32), active: bool, window: &mut Window) {
    let r = 4.0;
    let bounds = Bounds::from_corners(
        point(px(at.0 - r), px(at.1 - r)),
        point(px(at.0 + r), px(at.1 + r)),
    );
    let background = if active {
        point_color()
    } else {
        hsla(0., 0., 0., 1.)
    };
    window.paint_quad(quad(
        bounds,
        px(0.),
        background,
        px(1.5),
        point_color(),
        BorderStyle::Solid,
    ));
}

/// A small diamond, for curve handles.
pub fn paint_curve_handle(at: (f32, f32), active: bool, window: &mut Window) {
    let r = if active { 5.0 } else { 3.5 };
    let mut b = PathBuilder::fill();
    b.add_polygon(
        &[
            point(px(at.0), px(at.1 - r)),
            point(px(at.0 + r), px(at.1)),
            point(px(at.0), px(at.1 + r)),
            point(px(at.0 - r), px(at.1)),
        ],
        true,
    );
    if let Ok(path) = b.build() {
        window.paint_path(path, curve_color());
    }
}

pub fn near(a: (f32, f32), b: (f32, f32)) -> bool {
    (a.0 - b.0).hypot(a.1 - b.1) <= GRAB
}

/// The new curve of a segment from `from` to `to` (levels, 0..1) after dragging
/// its curve `up` pixels: dragging up always bends the middle of the segment up.
pub fn dragged_curve(start: f64, up: f32, from: f64, to: f64) -> f64 {
    let direction = (to - from).signum();
    (start - (up / CURVE_DRAG) as f64 * direction).clamp(-1.0, 1.0)
}

/// Round to a number of decimals, so dragged values are stored as `0.6873`
/// rather than `0.6872819066047668`.
pub fn round(x: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (x * scale).round() / scale
}

/// A position in pixels.
pub fn xy(p: Point<Pixels>) -> (f32, f32) {
    (p.x.as_f32(), p.y.as_f32())
}

/// What's shown for a resource whose file doesn't exist yet.
pub fn missing(
    file: String,
    on_create: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_2()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(format!("{file} doesn't exist yet"))
        .child(Button::new("create").label("Create it").on_click(on_create))
        .into_any_element()
}

/// A duration the way Ableton shows them: "1.00 ms", "600 ms", "2.50 s".
pub fn format_seconds(seconds: f64) -> String {
    if seconds < 0.01 {
        format!("{:.2} ms", seconds * 1000.0)
    } else if seconds < 1.0 {
        format!("{:.0} ms", seconds * 1000.0)
    } else if seconds < 10.0 {
        format!("{seconds:.2} s")
    } else {
        format!("{seconds:.1} s")
    }
}

#[cfg(test)]
mod tests {
    use super::{dragged_curve, format_seconds};

    #[test]
    fn durations() {
        assert_eq!(format_seconds(0.001), "1.00 ms");
        assert_eq!(format_seconds(0.6), "600 ms");
        assert_eq!(format_seconds(2.5), "2.50 s");
        assert_eq!(format_seconds(12.0), "12.0 s");
    }

    #[test]
    fn dragging_up_bends_the_middle_up() {
        let mid = |from: f64, to: f64, c: f64| rocktober_engine::curve::segment(from, to, 0.5, c);
        for (from, to) in [(0.0, 1.0), (1.0, 0.2)] {
            let bent = dragged_curve(0.0, 30.0, from, to);
            assert!(mid(from, to, bent) > mid(from, to, 0.0));
        }
        assert_eq!(dragged_curve(0.9, 500.0, 1.0, 0.0), 1.0);
    }
}
