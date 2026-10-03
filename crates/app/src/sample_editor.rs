//! The editor for `sample("...")`: the waveform, its length, and the time under
//! the mouse, which a click copies. A missing sample can be dropped in.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::*;
use rocktober_engine::resource::{self, ResourceKind};
use rocktober_engine::sample::{self, Overview};

use crate::ResourceEvent;

/// Overviews by file and modification time, shared by all sample editors, so
/// moving the cursor back and forth doesn't decode a file again.
pub type OverviewCache = Rc<RefCell<HashMap<(PathBuf, Option<SystemTime>), Arc<Overview>>>>;

enum State {
    Loading,
    Loaded(Arc<Overview>),
    Missing,
    Failed(String),
}

pub struct SampleEditor {
    name: String,
    /// Where a dropped file is copied to, if the sample doesn't exist.
    target: PathBuf,
    state: State,
    /// Times among the call's arguments (start and maybe end), shaded.
    times: Vec<f64>,
    /// The time under the mouse.
    hover: Option<f64>,
    /// Where the waveform was last drawn, to turn mouse positions into times.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    cache: OverviewCache,
}

impl EventEmitter<ResourceEvent> for SampleEditor {}

/// A time in the language's own format, `m:ss:mmm`, so it can be pasted
/// straight into the code.
pub fn format_time(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    format!("{}:{:02}:{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

impl SampleEditor {
    /// `dirs` are where samples are looked for, and `root` is the folder of the
    /// code file, whose `samples/` a dropped file is copied into.
    pub fn new(
        name: &str,
        times: Vec<f64>,
        dirs: &[PathBuf],
        root: &Path,
        cache: OverviewCache,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self {
            name: name.to_string(),
            target: ResourceKind::Sample.path(root, name),
            state: State::Missing,
            times,
            hover: None,
            bounds: Rc::new(Cell::new(Bounds::default())),
            cache,
        };
        if let Some(path) = resource::find(dirs, name) {
            editor.load(path, cx);
        }
        editor
    }

    pub fn set_times(&mut self, times: Vec<f64>, cx: &mut Context<Self>) {
        if times != self.times {
            self.times = times;
            cx.notify();
        }
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self.state, State::Loaded(_))
    }

    fn load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let key = (path.clone(), modified);
        if let Some(overview) = self.cache.borrow().get(&key) {
            self.state = State::Loaded(overview.clone());
            return;
        }
        self.state = State::Loading;
        let task = cx
            .background_executor()
            .spawn(async move { sample::overview(&path) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(overview) => {
                        let overview = Arc::new(overview);
                        this.cache.borrow_mut().insert(key, overview.clone());
                        State::Loaded(overview)
                    }
                    Err(e) => State::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy a dropped file to where the code expects the sample.
    fn add(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let Some(source) = paths.paths().first() else {
            return;
        };
        let ext = |p: &Path| p.extension().map(|e| e.to_string_lossy().to_lowercase());
        if ext(source) != ext(&self.target) {
            cx.emit(ResourceEvent::Error(format!(
                "{} needs a .{} file",
                self.name,
                ext(&self.target).unwrap_or_default()
            )));
            return;
        }
        let copied = self
            .target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::copy(source, &self.target));
        match copied {
            Ok(_) => {
                cx.emit(ResourceEvent::Info(format!(
                    "added {}",
                    self.target.display()
                )));
                self.load(self.target.clone(), cx);
            }
            Err(e) => cx.emit(ResourceEvent::Error(format!("can't copy the sample: {e}"))),
        }
        cx.notify();
    }

    fn time_at(&self, position: Point<Pixels>) -> Option<f64> {
        let State::Loaded(overview) = &self.state else {
            return None;
        };
        let bounds = self.bounds.get();
        if !bounds.contains(&position) || bounds.size.width <= px(0.) {
            return None;
        }
        let fraction = (position.x - bounds.origin.x).as_f32() / bounds.size.width.as_f32();
        Some(fraction.clamp(0.0, 1.0) as f64 * overview.seconds())
    }

    fn render_waveform(&self, overview: &Arc<Overview>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wave = theme.primary;
        let shade = theme.primary.opacity(0.15);
        let line = theme.foreground.opacity(0.7);
        let overview = overview.clone();
        let times = self.times.clone();
        let hover = self.hover;
        let bounds_cell = self.bounds.clone();

        let canvas = canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, (), window, _| {
                let left = bounds.origin.x.as_f32();
                let top = bounds.origin.y.as_f32();
                let width = bounds.size.width.as_f32();
                let height = bounds.size.height.as_f32();
                let seconds = overview.seconds();
                let x_of = |t: f64| left + (t / seconds).clamp(0.0, 1.0) as f32 * width;
                let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
                    Bounds::from_corners(point(px(x0), px(y0)), point(px(x1), px(y1)))
                };

                if let Some(&start) = times.first() {
                    let end = times.get(1).copied().unwrap_or(seconds);
                    window.paint_quad(fill(rect(x_of(start), top, x_of(end), top + height), shade));
                }

                // One bar per pixel column, from the lowest to the highest peak in it.
                let peaks = &overview.peaks;
                let columns = width.floor().max(1.0) as usize;
                let mid = top + height / 2.0;
                let half = height / 2.0;
                for x in 0..columns {
                    let a = x * peaks.len() / columns;
                    let b = ((x + 1) * peaks.len() / columns).clamp(a + 1, peaks.len());
                    if a >= peaks.len() {
                        break;
                    }
                    let (lo, hi) = peaks[a..b]
                        .iter()
                        .fold((0f32, 0f32), |(lo, hi), p| (lo.min(p.0), hi.max(p.1)));
                    let y0 = mid - hi.min(1.0) * half;
                    let y1 = (mid - lo.max(-1.0) * half).max(y0 + 1.0);
                    let x = left + x as f32;
                    window.paint_quad(fill(rect(x, y0, x + 1.0, y1), wave));
                }

                if let Some(t) = hover {
                    let x = x_of(t);
                    window.paint_quad(fill(rect(x, top, x + 1.0, top + height), line));
                }
            },
        )
        .size_full();

        div()
            .id("waveform")
            .flex_1()
            .min_h_0()
            .cursor_pointer()
            .child(canvas)
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let hover = this.time_at(e.position);
                if hover != this.hover {
                    this.hover = hover;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.hover.is_some() {
                    this.hover = None;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    if let Some(t) = this.time_at(e.position) {
                        let time = format_time(t);
                        cx.write_to_clipboard(ClipboardItem::new_string(time.clone()));
                        cx.emit(ResourceEvent::Info(format!("copied {time}")));
                    }
                }),
            )
            .into_any_element()
    }
}

impl Render for SampleEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mut header = h_flex().gap_3().text_xs().text_color(muted).child(
            div()
                .text_color(theme.foreground)
                .child(format!("sample \"{}\"", self.name)),
        );
        if let State::Loaded(overview) = &self.state {
            header = header.child(format!("{} long", format_time(overview.seconds())));
            header = header.child(div().flex_1()).child(match self.hover {
                Some(t) => format!("{}  (click to copy)", format_time(t)),
                None => String::new(),
            });
        }

        let message = |text: String, color: Hsla| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let body = match &self.state {
            State::Loaded(overview) => self.render_waveform(&overview.clone(), cx),
            State::Loading => message("loading…".into(), muted),
            State::Failed(e) => message(e.clone(), theme.danger),
            State::Missing => {
                let hover_bg = theme.primary.opacity(0.15);
                let border = theme.border;
                div()
                    .id("drop")
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_dashed()
                    .border_color(border)
                    .rounded_md()
                    .text_sm()
                    .text_color(muted)
                    .child(format!(
                        "{}/{} doesn't exist: drop an audio file here to add it",
                        ResourceKind::Sample.dir(),
                        self.name
                    ))
                    .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(hover_bg))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.add(paths, cx)))
                    .into_any_element()
            }
        };

        v_flex().size_full().gap_2().child(header).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn times_use_the_language_format() {
        assert_eq!(format_time(2.254), "0:02:254");
        assert_eq!(format_time(83.0), "1:23:000");
        assert_eq!(format_time(0.0005), "0:00:001");
        assert_eq!(format_time(59.9996), "1:00:000");
    }
}
