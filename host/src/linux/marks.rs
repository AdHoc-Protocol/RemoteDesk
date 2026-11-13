// The marks of the viewers on Linux. On X11: an override-redirect window of each stroke, of 32 bits - its image clear around the
// stroke, as the compositor shows it -, whose shape for the input is empty: the mouse goes through it. Without a compositor the X
// server shows no window clear, and Wayland lets no program put a window over the others: the user of the machine is told then.
// To fade, the image is put again, its pixels weaker: an image of a stroke is small.

use std::cell::RefCell;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shape;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, ImageFormat, StackMode, VisualClass, WindowClass};
use x11rb::rust_connection::RustConnection;

use super::video::Display;

/// How large the marks are on a display of `size`: one of 1080 lines is 1.
pub fn scale(size: (u32, u32)) -> f32 { (size.0.min(size.1) as f32 / 1080.0).max(1.0) }

/// The order of the bytes of an image: B, G, R, A - a pixel of the visual of 32 bits, little-endian.
pub const RGBA: bool = false;

/// The connection of the thread of the marks to the X server, with what its windows are made of.
struct X {
    conn:     RustConnection,
    root:     u32,
    visual:   u32,
    colormap: u32,
    /// The longest request the server takes, in bytes.
    max:      usize,
}

thread_local! {
    static X11: RefCell<Option<Result<X, String>>> = const { RefCell::new(None) };
}

fn open() -> Result<X, String> {
    if super::wayland() {
        return Err(String::from("Wayland lets no program draw over the others"));
    }
    let error = |e: &dyn std::fmt::Display| format!("the X server: {e}");
    let (conn, n) = x11rb::connect(None).map_err(|e| error(&e))?;
    let screen = conn.setup().roots[n].clone();
    let name = format!("_NET_WM_CM_S{n}");
    let atom = conn.intern_atom(false, name.as_bytes()).map_err(|e| error(&e))?.reply().map_err(|e| error(&e))?.atom;
    if conn.get_selection_owner(atom).map_err(|e| error(&e))?.reply().map_err(|e| error(&e))?.owner == x11rb::NONE {
        return Err(String::from("no compositor runs, and without one the X server shows no window clear"));
    }
    let visual = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
        .ok_or("the X server has no visual of 32 bits")?;
    let colormap = conn.generate_id().map_err(|e| error(&e))?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual).map_err(|e| error(&e))?;
    conn.xfixes_query_version(5, 0).map_err(|e| error(&e))?.reply().map_err(|e| error(&e))?;
    let max = conn.maximum_request_bytes();
    Ok(X { conn, root: screen.root, visual, colormap, max })
}

/// Calls `f` with the connection of this thread, opened the first time.
fn with<T>(f: impl FnOnce(&X) -> T) -> Result<T, String> {
    X11.with(|x| {
        let mut x = x.borrow_mut();
        let x = x.get_or_insert_with(open);
        x.as_ref().map(f).map_err(|e| e.clone())
    })
}

/// The window of one stroke, with the image it shows.
pub struct Layer {
    window: u32,
    gc:     u32,
    mapped: bool,
    width:  u32,
    height: u32,
    pixels: Vec<u8>,
    /// The pixels made weaker to fade.
    faded:  Vec<u8>,
}

impl Layer {
    pub fn new() -> Result<Self, String> {
        with(|x| -> Result<Self, String> {
            let error = |e: &dyn std::fmt::Display| format!("the X server: {e}");
            let c = &x.conn;
            let window = c.generate_id().map_err(|e| error(&e))?;
            let aux = CreateWindowAux::new().background_pixel(0).border_pixel(0).override_redirect(1).colormap(x.colormap);
            c.create_window(32, window, x.root, 0, 0, 1, 1, 0, WindowClass::INPUT_OUTPUT, x.visual, &aux).map_err(|e| error(&e))?;
            // no input: the mouse goes to the windows under it
            let region = c.generate_id().map_err(|e| error(&e))?;
            c.xfixes_create_region(region, &[]).map_err(|e| error(&e))?;
            c.xfixes_set_window_shape_region(window, shape::SK::INPUT, 0, 0, region).map_err(|e| error(&e))?;
            c.xfixes_destroy_region(region).map_err(|e| error(&e))?;
            let gc = c.generate_id().map_err(|e| error(&e))?;
            c.create_gc(gc, window, &CreateGCAux::new()).map_err(|e| error(&e))?;
            Ok(Self { window, gc, mapped: false, width: 0, height: 0, pixels: Vec::new(), faded: Vec::new() })
        })?
    }

    /// Shows `pixels`, `width` × `height`, at `x`, `y` of `display`, of opacity `alpha`.
    pub fn show(&mut self, display: &Display, x: i32, y: i32, width: u32, height: u32, pixels: &[u8], alpha: f32) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.extend_from_slice(pixels);
        let (window, mapped) = (self.window, self.mapped);
        let _ = with(|xc| {
            let place = ConfigureWindowAux::new().x(display.x + x).y(display.y + y).width(width).height(height).stack_mode(StackMode::ABOVE);
            let _ = xc.conn.configure_window(window, &place);
            if !mapped {
                let _ = xc.conn.map_window(window);
            }
        });
        self.mapped = true;
        self.put(alpha);
    }

    /// The opacity of what it shows.
    pub fn fade(&mut self, alpha: f32) { self.put(alpha); }

    /// The image of opacity `alpha`, in requests the server takes.
    fn put(&mut self, alpha: f32) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let data: &[u8] = if alpha >= 1.0 {
            &self.pixels
        } else {
            let a = alpha.max(0.0);
            self.faded.clear();
            self.faded.extend(self.pixels.iter().map(|&b| (b as f32 * a) as u8)); // premultiplied: each byte the same
            &self.faded
        };
        let (window, gc, width, height) = (self.window, self.gc, self.width, self.height);
        let _ = with(|xc| {
            let row = width as usize * 4;
            let rows = ((xc.max.saturating_sub(64)) / row).max(1);
            let mut y = 0;
            while y < height as usize {
                let n = rows.min(height as usize - y);
                let _ = xc.conn.put_image(ImageFormat::Z_PIXMAP, window, gc, width as u16, n as u16, 0, y as i16, 0, 32, &data[y * row..(y + n) * row]);
                y += n;
            }
        });
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        let (window, gc) = (self.window, self.gc);
        let _ = with(|x| {
            let _ = x.conn.free_gc(gc);
            let _ = x.conn.destroy_window(window);
            let _ = x.conn.flush();
        });
    }
}

/// What the layers asked of the X server goes to it.
pub fn pump() {
    X11.with(|x| {
        if let Some(Ok(x)) = x.borrow().as_ref() {
            let _ = x.conn.flush();
        }
    });
}
