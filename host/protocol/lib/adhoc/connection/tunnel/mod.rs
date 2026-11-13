// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! A virtual connection that rides the frames of a carrier: a relay passes the frames of a tunnel
//! on between two carriers ([`relay`]), an end point is the external connection of the virtual one
//! ([`end_point`]).

pub mod end_point;
pub mod parked;
pub mod relay;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::lib::adhoc::connection::external::External;

/// The address of this host in the tunnels it ends and relays: the identity its peers know it by. 0 - none: the usual route of a
/// tunnel ([`relay::Relay::route`]) ends here only what comes on a dialed connection or from a host not identified, and the usual side ([`side`])
/// is off.
pub static ADDRESS: AtomicU32 = AtomicU32::new(0);

/// The hosts this one relays tunnels for ([`hosts`]).
static HOSTS: OnceLock<Arc<dyn Hosts>> = OnceLock::new();

/// The hosts a relay sends tunnels to, by their address.
pub trait Hosts: Send + Sync {
    /// The generated connection of the host of `address`, the one the frame of the tunnel is queued on ( `downcast` to it in the
    /// region Tunneling ); `None` - none now.
    fn at(&self, address: u32) -> Option<Arc<dyn std::any::Any + Send + Sync>>;
}

/// Sets the hosts this one relays tunnels for, by the address their login proved ([`External::identify`]):
/// [`crate::lib::adhoc::connection::sessions::Sessions`] is one. Set this, once, before the first connection: a second call
/// changes nothing. Not set - the usual route of a tunnel is off.
pub fn hosts(h: Arc<dyn Hosts>) {
    let _ = HOSTS.set(h);
}

/// The first of the labels ([`labels`]).
pub(crate) static LABEL_FIRST: AtomicU32 = AtomicU32::new(0);
/// Half the labels: a label is `first + 2 * i + side`, the side that dialed the connection gives the odd ones, so the two never meet.
pub(crate) static LABEL_PAIRS: AtomicU32 = AtomicU32::new(0);

/// The tunnel ids this host gives the circuits it relays ([`relay::Relay::through`]): `count` ids from `first`, apart from every
/// address the hosts of these tunnels use. A tunnel through two relays or more needs them on each relay of its route: one tunnel id on
/// a connection names one tunnel, so a relay that sends a tunnel on to another relay names it there by a label of its own and
/// remembers what the label stands for, as a Tor circuit or an SSH channel does. Not set - this host relays no circuit.
pub fn labels(first: u32, count: u32) {
    LABEL_FIRST.store(first, Ordering::Relaxed);
    LABEL_PAIRS.store(count >> 1, Ordering::Relaxed);
}

/// The hosts set by [`hosts`]; `None` - none.
#[inline]
pub(crate) fn hosts_() -> Option<&'static Arc<dyn Hosts>> {
    HOSTS.get()
}

/// The usual side of a tunnel between two hosts of one type, by what both ends know: the end of the lesser [`ADDRESS`] is left
/// ( false ). The far end is the one `tunnel_id` names - in a tunnel that ends on the relay itself it is the relay's own address,
/// and the far end is the host on `carrier`. `None` - no [`ADDRESS`]: the side goes by who opened the tunnel.
#[inline]
pub fn side(tunnel_id: u32, carrier: &dyn External) -> Option<bool> {
    let a = ADDRESS.load(Ordering::Relaxed);
    if a == 0 {
        return None;
    }
    Some(a > if tunnel_id == a { carrier.id() } else { tunnel_id })
}
