// The marks of the viewers over the screen of the machine: a line drawn by hand, a ring where the viewer tapped, an arrow - for its
// user to see where to tap, which field to fill. Each shows at once, holds a moment and fades by itself; the viewer sees it in the
// video as the user sees it.
//
// A stroke is drawn here, into an image of its own size ( its frame on the display, not the display ), and the system shows the image
// over everything in a layer of its own that the mouse and the touches go through ( `os::marks::Layer` ). The image is drawn again
// only when the stroke changes - points came, the ring ripples -; to fade, the layer only changes its opacity.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};
use crate::os::marks::{pump, Layer, RGBA};
use crate::os::video::Display;

/// The colours the page offers, by `Mark.color`: red, yellow, green, blue.
const COLORS: [[u8; 3]; 4] = [[0xF0, 0x3E, 0x3E], [0xFF, 0xC4, 0x1A], [0x2E, 0xC2, 0x5A], [0x3B, 0x82, 0xF6]];
/// A drawn stroke stays this long, then fades for `FADE`.
const HOLD: Duration = Duration::from_millis(1400);
const FADE: Duration = Duration::from_millis(1100);
/// A stroke not finished and not drawn on for this long is finished: its viewer is gone, or lost its pack.
const STALE: Duration = Duration::from_secs(5);
/// One ripple of a ring.
const RIPPLE: Duration = Duration::from_millis(900);
/// At most at once: an older one goes for a new one.
const STROKES: usize = 32;
const POINTS: usize = 8192;
/// The frames of the layers: what changes is drawn no oftener.
const TICK: Duration = Duration::from_millis(33);

/// A stroke as the viewer sends it, its points in pixels of its display.
struct Stroke {
    id:      u64,
    owner:   usize,
    number:  u8,
    kind:    Viewer_Mark_Kind,
    color:   [u8; 3],
    /// Its display, and the size of it in pixels.
    screen:  u8,
    size:    (u32, u32),
    points:  Vec<(f32, f32)>,
    born:    Instant,
    last:    Instant,
    done:    Option<Instant>,
    /// Counts the changes: the layer draws the image again when it has not drawn this one.
    version: u64,
}

impl Stroke {
    /// When it began to hold: drawn, or silent for `STALE`.
    fn finished(&self) -> Option<Instant> { self.done.or_else(|| (self.last.elapsed() >= STALE).then_some(self.last)) }

    /// Its opacity now: 0 - gone.
    fn alpha(&self, now: Instant) -> f32 {
        match self.finished() {
            None => 1.0,
            Some(at) => {
                let t = now.saturating_duration_since(at);
                if t <= HOLD { 1.0 } else { 1.0 - (t - HOLD).as_secs_f32() / FADE.as_secs_f32() }.max(0.0)
            }
        }
    }
}

struct Shared {
    strokes: Vec<Stroke>,
    next_id: u64,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { strokes: Vec::new(), next_id: 1 });
static WAKE: Condvar = Condvar::new();

pub fn register() {
    actor0::working::on_receive_d::Viewer_Mark.add(on_mark);
}

/// A piece of a stroke of a viewer - any viewer, not only the one that controls.
fn on_mark(mark: &mut Viewer_Mark, conn: &Connection, _actor: &Actor0) {
    let owner = conn as *const Connection as usize;
    let now = Instant::now();
    let points = mark.points.take().unwrap_or_default();
    let mut s = SHARED.lock().unwrap();
    let at = s.strokes.iter().position(|k| k.owner == owner && k.number == mark.stroke && k.done.is_none());
    let i = match at {
        Some(i) => i,
        None => {
            let Some(display) = crate::os::video::displays().into_iter().find(|d| d.screen == mark.screen()) else { return };
            if s.strokes.len() >= STROKES {
                s.strokes.remove(0);
            }
            let id = s.next_id;
            s.next_id += 1;
            s.strokes.push(Stroke {
                id,
                owner,
                number: mark.stroke,
                kind: mark.kind(),
                color: COLORS[(mark.color() as usize).min(COLORS.len() - 1)],
                screen: display.screen,
                size: (display.width, display.height),
                points: Vec::new(),
                born: now,
                last: now,
                done: None,
                version: 0,
            });
            starts();
            s.strokes.len() - 1
        }
    };
    let k = &mut s.strokes[i];
    let (w, h) = ((k.size.0.max(1) - 1) as f32 / 65535.0, (k.size.1.max(1) - 1) as f32 / 65535.0);
    for pair in points.chunks_exact(2) {
        if k.points.len() >= POINTS {
            break;
        }
        k.points.push((pair[0] as f32 * w, pair[1] as f32 * h));
    }
    k.last = now;
    if mark.done() {
        k.done = Some(now);
    }
    k.version += 1;
    drop(s);
    WAKE.notify_one();
}

/// The thread of the layers, once a first mark comes.
fn starts() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new().name("marks".into()).spawn(run);
    });
}

/// What the thread draws of a stroke this turn.
struct Job {
    id:      u64,
    version: u64,
    alpha:   f32,
    /// Its points, when the image is to be drawn again.
    draw:    Option<Drawn>,
}

struct Drawn {
    kind:    Viewer_Mark_Kind,
    color:   [u8; 3],
    screen:  u8,
    size:    (u32, u32),
    points:  Vec<(f32, f32)>,
    /// How far the ripple of a ring is: 0 … 1.
    ripple:  f32,
}

/// A layer of a stroke, as the thread keeps it.
struct Shown {
    id:      u64,
    layer:   Layer,
    version: u64,
    alpha:   f32,
}

/// The thread of the layers: the layers are its own - the windows of some systems answer to the thread that made them.
fn run() {
    let mut shown: Vec<Shown> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    let mut image = Image::default();
    let mut told = false;
    // the displays as the system tells them: asked again for a new layer only, it may be slow to ask
    let mut displays: Vec<Display> = Vec::new();
    loop {
        let now = Instant::now();
        {
            let mut s = SHARED.lock().unwrap();
            s.strokes.retain(|k| k.alpha(now) > 0.0);
            while s.strokes.is_empty() {
                shown.clear(); // the layers go with their strokes
                pump();
                s = WAKE.wait(s).unwrap();
            }
            let now = Instant::now();
            for k in &s.strokes {
                let ring = k.kind == Viewer_Mark_Kind::RING;
                let known = shown.iter().find(|v| v.id == k.id).map(|v| v.version);
                let draw = (known != Some(k.version) || ring).then(|| Drawn {
                    kind: k.kind,
                    color: k.color,
                    screen: k.screen,
                    size: k.size,
                    points: k.points.clone(),
                    ripple: (now.saturating_duration_since(k.born).as_secs_f32() / RIPPLE.as_secs_f32()).fract(),
                });
                jobs.push(Job { id: k.id, version: k.version, alpha: k.alpha(now), draw });
            }
        }
        shown.retain(|v| jobs.iter().any(|j| j.id == v.id));
        for job in jobs.drain(..) {
            let at = match shown.iter().position(|v| v.id == job.id) {
                Some(at) => at,
                None => match Layer::new() {
                    Ok(layer) => {
                        displays = crate::os::video::displays();
                        shown.push(Shown { id: job.id, layer, version: 0, alpha: -1.0 });
                        shown.len() - 1
                    }
                    Err(why) => {
                        if !told {
                            told = true;
                            say!("✏️", Warn, "The marks of the viewer are not shown here: {why}");
                        }
                        continue;
                    }
                },
            };
            let v = &mut shown[at];
            if let Some(d) = job.draw {
                let display = displays.iter().find(|x| x.screen == d.screen);
                if let (Some(display), Some((x, y))) = (display, image.draw(&d)) {
                    v.layer.show(display, x, y, image.width, image.height, &image.pixels, job.alpha);
                    v.alpha = job.alpha;
                }
                v.version = job.version;
            }
            if (v.alpha - job.alpha).abs() > 0.004 {
                v.layer.fade(job.alpha);
                v.alpha = job.alpha;
            }
        }
        pump();
        std::thread::sleep(TICK);
    }
}

// ── the image of a stroke ──

/// The image a stroke is drawn into, kept from one to the next: premultiplied, 4 bytes a pixel in the order of the system.
#[derive(Default)]
struct Image {
    width:  u32,
    height: u32,
    pixels: Vec<u8>,
    /// How much of each pixel the colour covers, and its dark edge: 0 … 1.
    inner:  Vec<f32>,
    outer:  Vec<f32>,
}

/// A line of the image: its points, its width, how strong.
struct Line {
    points: Vec<(f32, f32)>,
    width:  f32,
    alpha:  f32,
}

impl Image {
    /// Draws `d`: where the image lies on its display, None - nothing of it is on the display.
    fn draw(&mut self, d: &Drawn) -> Option<(i32, i32)> {
        let lines = shape(d);
        let edge = 3.0 * scale(d.size); // the dark edge around the colour: seen on any background
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for l in &lines {
            let r = l.width / 2.0 + edge + 2.0;
            for &(x, y) in &l.points {
                x0 = x0.min(x - r);
                y0 = y0.min(y - r);
                x1 = x1.max(x + r);
                y1 = y1.max(y + r);
            }
        }
        let x0 = (x0.floor() as i32).max(0);
        let y0 = (y0.floor() as i32).max(0);
        let x1 = (x1.ceil() as i32).min(d.size.0 as i32);
        let y1 = (y1.ceil() as i32).min(d.size.1 as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        self.width = (x1 - x0) as u32;
        self.height = (y1 - y0) as u32;
        let n = (self.width * self.height) as usize;
        for buffer in [&mut self.inner, &mut self.outer] {
            buffer.clear();
            buffer.resize(n, 0.0);
        }
        for l in &lines {
            let half = l.width / 2.0;
            self.cover(&l.points, half + edge, l.alpha, x0, y0, false);
            self.cover(&l.points, half, l.alpha, x0, y0, true);
        }
        self.pixels.clear();
        self.pixels.reserve(n * 4);
        let [r, g, b] = d.color;
        for i in 0..n {
            let a = self.inner[i];
            let o = self.outer[i] * 0.45 * (1.0 - a); // the edge, black, under the colour
            let alpha = a + o;
            let (cr, cg, cb) = ((r as f32 * a) as u8, (g as f32 * a) as u8, (b as f32 * a) as u8);
            let al = (alpha * 255.0) as u8;
            if RGBA {
                self.pixels.extend_from_slice(&[cr, cg, cb, al]);
            } else {
                self.pixels.extend_from_slice(&[cb, cg, cr, al]);
            }
        }
        Some((x0, y0))
    }

    /// Covers the pixels within `half` of the line through `points`, smoothed over a pixel: each pixel keeps the most it got.
    fn cover(&mut self, points: &[(f32, f32)], half: f32, alpha: f32, x0: i32, y0: i32, inner: bool) {
        let (w, h) = (self.width as i32, self.height as i32);
        let buffer = if inner { &mut self.inner } else { &mut self.outer };
        let single = [points[0], points[0]];
        let segments = if points.len() == 1 { &single[..] } else { points };
        for pair in segments.windows(2) {
            let ((ax, ay), (bx, by)) = ((pair[0].0 - x0 as f32, pair[0].1 - y0 as f32), (pair[1].0 - x0 as f32, pair[1].1 - y0 as f32));
            let r = half + 1.0;
            let (sx0, sy0) = (((ax.min(bx) - r).floor() as i32).max(0), ((ay.min(by) - r).floor() as i32).max(0));
            let (sx1, sy1) = (((ax.max(bx) + r).ceil() as i32).min(w), ((ay.max(by) + r).ceil() as i32).min(h));
            let (dx, dy) = (bx - ax, by - ay);
            let length = dx * dx + dy * dy;
            for py in sy0..sy1 {
                let row = (py * w) as usize;
                let cy = py as f32 + 0.5;
                for px in sx0..sx1 {
                    let cx = px as f32 + 0.5;
                    // the distance to the segment
                    let t = if length > 0.0 { (((cx - ax) * dx + (cy - ay) * dy) / length).clamp(0.0, 1.0) } else { 0.0 };
                    let (ex, ey) = (ax + t * dx - cx, ay + t * dy - cy);
                    let c = (half + 0.5 - (ex * ex + ey * ey).sqrt()).clamp(0.0, 1.0) * alpha;
                    let cell = &mut buffer[row + px as usize];
                    if c > *cell {
                        *cell = c;
                    }
                }
            }
        }
    }
}

/// How large the marks are on a display of `size`: as the system sees it ( a phone by the density of its screen ).
fn scale(size: (u32, u32)) -> f32 { crate::os::marks::scale(size) }

/// The lines a stroke is made of.
fn shape(d: &Drawn) -> Vec<Line> {
    let s = scale(d.size);
    let width = 6.0 * s;
    let Some(&first) = d.points.first() else { return Vec::new() };
    match d.kind {
        Viewer_Mark_Kind::RING => {
            let radius = 26.0 * s;
            let circle = |r: f32| (0..=48).map(|i| i as f32 * std::f32::consts::TAU / 48.0).map(|a| (first.0 + r * a.cos(), first.1 + r * a.sin())).collect::<Vec<_>>();
            vec![
                Line { points: circle(radius), width, alpha: 1.0 },
                // the ripple: away from the ring, weaker as it goes
                Line { points: circle(radius * (1.0 + d.ripple)), width: width * 0.6, alpha: 1.0 - d.ripple },
            ]
        }
        Viewer_Mark_Kind::ARROW => {
            let last = *d.points.last().unwrap();
            let (dx, dy) = (last.0 - first.0, last.1 - first.1);
            let length = (dx * dx + dy * dy).sqrt();
            if length < 1.0 {
                return vec![Line { points: vec![first], width, alpha: 1.0 }];
            }
            let head = (22.0 * s).min(length * 0.6);
            let (ux, uy) = (dx / length, dy / length);
            let wing = |turn: f32| {
                let (c, n) = (turn.cos(), turn.sin());
                (last.0 - head * (ux * c - uy * n), last.1 - head * (uy * c + ux * n))
            };
            vec![Line { points: vec![first, last], width, alpha: 1.0 }, Line { points: vec![wing(0.5), last, wing(-0.5)], width, alpha: 1.0 }]
        }
        _ => vec![Line { points: d.points.clone(), width, alpha: 1.0 }],
    }
}
