// The X server: one connection for the life of the program, shared by the capture and the input - x11rb serializes it.

use std::sync::LazyLock;

use x11rb::connection::Connection;
use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct X {
    pub conn: RustConnection,
    pub root: Window,
    /// The extensions the server has.
    pub shm:    bool,
    pub damage: bool,
    pub xfixes: bool,
    pub xtest:  bool,
    pub randr:  bool,
}

static X_: LazyLock<Option<X>> = LazyLock::new(|| {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen].root;
    let shm = conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
    let damage = conn.damage_query_version(1, 1).ok().and_then(|c| c.reply().ok()).is_some();
    let xfixes = conn.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).is_some();
    let xtest = conn.xtest_get_version(2, 2).ok().and_then(|c| c.reply().ok()).is_some();
    let randr = conn.randr_query_version(1, 5).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.major_version > 1 || r.minor_version >= 5);
    Some(X { conn, root, shm, damage, xfixes, xtest, randr })
});

/// The X server, when it answers.
pub fn x() -> Option<&'static X> { X_.as_ref() }

/// A monitor of the X server: ( name, x, y, width, height, primary ).
pub struct Monitor {
    pub name:    String,
    pub x:       i32,
    pub y:       i32,
    pub width:   u32,
    pub height:  u32,
    pub primary: bool,
    pub outputs: Vec<u32>,
}

/// The monitors as RandR tells them; the whole screen as one, when RandR is too old.
pub fn monitors() -> Vec<Monitor> {
    let Some(x) = x() else { return Vec::new() };
    if x.randr {
        if let Some(reply) = x.conn.randr_get_monitors(x.root, true).ok().and_then(|c| c.reply().ok()) {
            return reply
                .monitors
                .iter()
                .map(|m| Monitor {
                    name: x.conn.get_atom_name(m.name).ok().and_then(|c| c.reply().ok()).map(|a| String::from_utf8_lossy(&a.name).into_owned()).unwrap_or_default(),
                    x: m.x as i32,
                    y: m.y as i32,
                    width: m.width as u32,
                    height: m.height as u32,
                    primary: m.primary,
                    outputs: m.outputs.clone(),
                })
                .collect();
        }
    }
    let screen = &x.conn.setup().roots[0];
    vec![Monitor {
        name: String::from("screen"),
        x: 0,
        y: 0,
        width: screen.width_in_pixels as u32,
        height: screen.height_in_pixels as u32,
        primary: true,
        outputs: Vec::new(),
    }]
}
