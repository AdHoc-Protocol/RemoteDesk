// The resolution of a display fitted to the window of the page ( `ClientResolution` ): the largest mode of the display that fits in
// the window. Only the viewer that controls the machine fits it. The change is temporary: a request of all zeros restores the
// resolution the display had, and so does the last viewer leaving, or the one that controls and fitted it - another one that only
// took the control keeps what it sees. The scale of the display is not changed. How a mode is chosen and set is the system's:
// `os::mode`.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};

/// The connection of the viewer that fitted the displays last, by its address; 0 - none.
static FITTED_BY: AtomicUsize = AtomicUsize::new(0);

pub fn register() {
    actor0::working::on_receive_d::Viewer_ClientResolution.add(on_resolution);
}

/// Every display a page changed gets its resolution back.
pub fn restore_all() {
    FITTED_BY.store(0, Ordering::Relaxed);
    crate::os::mode::restore_all();
}

/// The viewer on `conn` left: the resolution goes back when `nobody` watches now, or when it controlled and fitted it.
pub fn left(conn: &Connection, controlled: bool, nobody: bool) {
    let fitted = FITTED_BY.compare_exchange(conn as *const Connection as usize, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok();
    if nobody || (controlled && fitted) {
        restore_all();
    }
}

fn on_resolution(request: Viewer_ClientResolution, conn: &Connection, _actor: &Actor0) {
    if !crate::session::controls(conn) {
        return;
    }
    FITTED_BY.store(conn as *const Connection as usize, Ordering::Relaxed);
    let displays = crate::os::video::displays();
    let Some(display) = displays.iter().find(|d| d.screen == request.screen()) else { return };
    if request.width() == 0 || request.height() == 0 {
        crate::os::mode::restore_one(display);
    } else {
        crate::os::mode::fit(display, request.width() as u32, request.height() as u32);
    }
}
