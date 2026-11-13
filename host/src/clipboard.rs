// The clipboard of the machine and the page: what is copied on the machine goes to the viewer that controls it, what that viewer's
// page copied is set on the machine - of the others, not. Text is `text/plain; charset=UTF-8` with `\n` lines on the wire, as Chrome
// Remote Desktop has it; a picture is PNG, as the clipboards of the browsers take it. How the clipboard of the system is watched and set
// is the system's: `os::clip`; the system tells a picture copied by `image_changed`.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};

/// As the description takes at most.
const MAX_BYTES: usize = 1 << 24;

/// The text both sides have now, hashed as it goes on the wire: a system may tell of one copy more than once, and what came
/// from the page does not go back.
static KNOWN: AtomicU64 = AtomicU64::new(0);

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

pub fn register() {
    actor0::working::on_receive_d::ClipboardEvent.add(on_clipboard);
    crate::os::clip::watch(changed);
}

/// The page that controls copied: its text goes to the clipboard of the machine.
fn on_clipboard(event: &mut ClipboardEvent, conn: &Connection, _actor: &Actor0) {
    if !crate::session::controls(conn) {
        return;
    }
    let Some(data) = event.data.take() else { return };
    KNOWN.store(hash(&data), Ordering::Release);
    if event.image() {
        if !crate::os::clip::set_image(&data) {
            say!("📋", Bad, "The picture from the page did not go into the clipboard");
        }
    } else if !crate::os::clip::set(&String::from_utf8_lossy(&data)) {
        say!("📋", Bad, "The text from the page did not go into the clipboard");
    }
}

/// A line the user of the machine sends on - the line for whoever connects, the answer to a page - put on the clipboard of the
/// machine, so that it is pasted into a message as it is: true - it is there. No clipboard ( no desktop, a console over SSH ) - false.
pub fn put_line(line: &str) -> bool {
    crate::os::clip::set(line)
}


/// The clipboard of the machine changed - not by the page: its text, with `\n` lines, goes to the viewer that controls.
fn changed(text: String) { send(text.into_bytes(), false); }

/// A picture was copied on the machine - not by the page: PNG, to the viewer that controls.
#[allow(dead_code)] // of the systems whose clipboard holds pictures
pub fn image_changed(png: Vec<u8>) { send(png, true); }

fn send(bytes: Vec<u8>, image: bool) {
    let Some(conn) = crate::session::controller() else { return };
    let h = hash(&bytes);
    if KNOWN.swap(h, Ordering::AcqRel) == h {
        return;
    }
    if bytes.len() > MAX_BYTES {
        return say!("📋", Warn, "{} bytes in the clipboard: more than the page takes", bytes.len());
    }
    let mut event = ClipboardEvent::default();
    event.data = Some(bytes);
    event.set_image(image);
    actor0::working::transmitter::send_clipboard_event(event, &conn);
}
