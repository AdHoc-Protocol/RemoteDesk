// The rights of an administrator, when the operator asks for them ( `ElevationRequest` ): on Windows no input of a program reaches
// a window of a program run as administrator unless it runs as one too.
//
// The host asks the system to start it again with those rights, with the same address, key and token: the user of the machine
// confirms. Confirmed, this host ends and the new one takes the port; the page dials again by itself and logs in with the same
// token. Each step is told to the page: `Elevation`. How the system is asked is its own: `os::rights`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};
use crate::os::rights::{self, Refused};

/// The prompt of the system is on the screen of the machine.
static ASKING: AtomicBool = AtomicBool::new(false);

pub fn register() {
    actor0::working::on_receive_d::Viewer_ElevationRequest.add(on_request);
}

/// The host runs with the rights of an administrator.
pub fn elevated_now() -> bool { rights::elevated() }

fn tell(conn: &Connection, state: Machine_Elevation_State) {
    actor0::working::transmitter::send_machine_elevation(Machine_Elevation::new(state), conn);
}

/// Only the viewer that controls the machine asks.
fn on_request(_request: &mut Viewer_ElevationRequest, conn: &Connection, _actor: &Actor0) {
    if !crate::session::controls(conn) {
        return;
    }
    if !rights::matter() {
        return tell(conn, Machine_Elevation_State::NOT_NEEDED);
    }
    if rights::elevated() {
        return tell(conn, Machine_Elevation_State::ALREADY);
    }
    if ASKING.swap(true, Ordering::AcqRel) {
        return tell(conn, Machine_Elevation_State::ASKING); // asked already: the prompt is there
    }
    let Some(conn) = crate::session::viewer(conn) else {
        ASKING.store(false, Ordering::Release);
        return;
    };
    tell(&conn, Machine_Elevation_State::ASKING);
    std::thread::Builder::new().name("elevate".into()).spawn(move || ask(conn)).expect("the thread of the elevation");
}

/// The prompt of the system: it waits for the user of the machine.
fn ask(conn: Arc<Connection>) {
    say!("🛡️", Wait, "The viewer asks for the rights of an administrator: confirm it on the screen");
    match rights::relaunch(&crate::session::relaunch_args()) {
        Ok(()) => {
            say!("🛡️", Good, "Confirmed: the program goes on as administrator, the page connects again");
            tell(&conn, Machine_Elevation_State::CONFIRMED);
            std::thread::sleep(Duration::from_millis(300)); // the answer goes out before this host ends
            crate::input::release_all();
            crate::resize::restore_all();
            std::process::exit(0);
        }
        Err(Refused::Declined) => {
            say!("🛡️", Quiet, "Declined");
            tell(&conn, Machine_Elevation_State::DECLINED);
        }
        Err(Refused::Failed(why)) => {
            say!("🛡️", Bad, "The prompt cannot be shown: {why}");
            tell(&conn, Machine_Elevation_State::FAILED);
        }
    }
    ASKING.store(false, Ordering::Release);
}
