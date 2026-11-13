// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Packed local connection lifecycle events.
//!
//! [`Event`] combines an action with orthogonal flags in a `u32`. Flags occupy the low byte; action
//! bits occupy bits 8 through 16. Predicates inspect these fields independently. Timeout events
//! carry the close action and a timeout flag. WebSocket readiness and graceful closing use their
//! own action categories.
//!
//! Events describe local callbacks. Their packed representation is not a protocol wire format.

/// Orthogonal lifecycle flags and the action-field mask.
pub mod mask {
    pub const REMOTE:    u32 = 1 << 0;
    pub const THIS:      u32 = 1 << 1;
    pub const GRACEFUL:  u32 = 1 << 2;
    pub const ABRUPT:    u32 = 1 << 3;
    pub const RECEIVE:   u32 = 1 << 4;
    pub const TRANSMIT:  u32 = 1 << 5;
    pub const WEBSOCKET: u32 = 1 << 6;
    /// Marks timeout events independently of their close action.
    pub const TIMEOUT:   u32 = 1 << 7;
    pub const ACTION:    u32 = 0x1_FF00; // bits 8..16 hold the Action bits (PROTOCOL_ERROR is bit 16)
}
pub use mask as Mask;

/// Action categories in bits 8 through 16. Each category occupies one bit.
pub mod action {
    pub const CONNECT:           u32 = 0x01_00;
    pub const CLOSE:             u32 = 0x02_00;
    /// The protocol above the socket is ready, including a completed WebSocket upgrade.
    pub const READY:             u32 = 0x04_00;
    /// A closing handshake is in progress while the connection remains open.
    pub const CLOSING:           u32 = 0x08_00;
    pub const PING:              u32 = 0x10_00;
    pub const PONG:              u32 = 0x20_00;
    pub const EMPTY_FRAME:       u32 = 0x40_00;
    pub const HANDSHAKE_FAILURE: u32 = 0x80_00;
    pub const PROTOCOL_ERROR:    u32 = 0x100_00;
}
pub use action as Action;

/// Connection lifecycle event combining [`action`] bits and [`mask`] flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Event(pub u32);

impl Event {
    // Composite TCP / general events -----------

    pub const REMOTE_CONNECT:        Event = Event(mask::REMOTE   | action::CONNECT);
    pub const THIS_CONNECT:          Event = Event(mask::THIS     | action::CONNECT);
    pub const REMOTE_CLOSE_GRACEFUL: Event = Event(mask::REMOTE   | mask::GRACEFUL | action::CLOSE);
    pub const THIS_CLOSE_GRACEFUL:   Event = Event(mask::THIS     | mask::GRACEFUL | action::CLOSE);
    pub const REMOTE_CLOSE_ABRUPTLY: Event = Event(mask::REMOTE   | mask::ABRUPT   | action::CLOSE);
    pub const THIS_CLOSE_ABRUPTLY:   Event = Event(mask::THIS     | mask::ABRUPT   | action::CLOSE);
    /// Timeout events carry both the close action and the timeout flag.
    pub const TRANSMIT_TIMEOUT:      Event = Event(mask::TIMEOUT | mask::TRANSMIT | action::CLOSE);
    pub const RECEIVE_TIMEOUT:       Event = Event(mask::TIMEOUT | mask::RECEIVE  | action::CLOSE);

    // Composite WebSocket events --------------------------------------

    /// WebSocket upgrade completion uses the ready action.
    pub const WEBSOCKET_THIS_CONNECT:          Event = Event(mask::WEBSOCKET | mask::THIS   | action::READY);
    pub const WEBSOCKET_REMOTE_CONNECTED:      Event = Event(mask::WEBSOCKET | mask::REMOTE | action::READY);
    /// Alias for [`WEBSOCKET_REMOTE_CONNECTED`](Self::WEBSOCKET_REMOTE_CONNECTED).
    pub const WEBSOCKET_REMOTE_CONNECT:        Event = Event::WEBSOCKET_REMOTE_CONNECTED;
    pub const WEBSOCKET_HANDSHAKE_FAILURE:     Event = Event(mask::WEBSOCKET | mask::REMOTE | mask::ABRUPT | action::HANDSHAKE_FAILURE);
    pub const WEBSOCKET_PROTOCOL_ERROR:        Event = Event(mask::WEBSOCKET | mask::REMOTE | mask::ABRUPT | action::PROTOCOL_ERROR);
    pub const WEBSOCKET_PING:                  Event = Event(mask::WEBSOCKET | mask::REMOTE | action::PING);
    pub const WEBSOCKET_PONG:                  Event = Event(mask::WEBSOCKET | mask::REMOTE | action::PONG);
    pub const WEBSOCKET_EMPTY_FRAME:           Event = Event(mask::WEBSOCKET | mask::REMOTE | action::EMPTY_FRAME);
    /// A graceful WebSocket close uses the closing action while the handshake is in progress.
    pub const WEBSOCKET_REMOTE_CLOSE_GRACEFUL: Event = Event(mask::WEBSOCKET | mask::REMOTE | mask::GRACEFUL | action::CLOSING);
    pub const WEBSOCKET_THIS_CLOSE_GRACEFUL:   Event = Event(mask::WEBSOCKET | mask::THIS   | mask::GRACEFUL | action::CLOSING);
    pub const WEBSOCKET_REMOTE_CLOSE_ABRUPTLY: Event = Event(mask::WEBSOCKET | mask::REMOTE | mask::ABRUPT   | action::CLOSE);
    pub const WEBSOCKET_THIS_CLOSE_ABRUPTLY:   Event = Event(mask::WEBSOCKET | mask::THIS   | mask::ABRUPT   | action::CLOSE);
    pub const WEBSOCKET_TRANSMIT_TIMEOUT:      Event = Event(mask::WEBSOCKET | mask::TIMEOUT | mask::TRANSMIT | action::CLOSE);
    pub const WEBSOCKET_RECEIVE_TIMEOUT:       Event = Event(mask::WEBSOCKET | mask::TIMEOUT | mask::RECEIVE  | action::CLOSE);

    // Predicates --

    /// Returns the packed local event representation.
    #[inline] pub const fn raw(self) -> u32 { self.0 }

    /// Returns the action bits selected by [`mask::ACTION`].
    #[inline] pub const fn action(self) -> u32 { self.0 & mask::ACTION }

    /// Returns whether the event has the connect action.
    #[inline] pub const fn is_connect(self)            -> bool { self.0 & action::CONNECT != 0 }
    /// Returns whether the event has the close action, including timeout events.
    #[inline] pub const fn is_close(self)              -> bool { self.0 & action::CLOSE != 0 }
    /// Returns whether the timeout flag is set.
    #[inline] pub const fn is_timeout(self)            -> bool { self.0 & mask::TIMEOUT != 0 }
    /// Returns whether the protocol above the socket is ready.
    #[inline] pub const fn is_ready(self)              -> bool { self.0 & action::READY != 0 }
    /// Returns whether a closing handshake is in progress.
    #[inline] pub const fn is_closing(self)            -> bool { self.0 & action::CLOSING != 0 }
    /// Returns whether the WebSocket handshake failed.
    #[inline] pub const fn is_handshake_failure(self)  -> bool { self.0 & action::HANDSHAKE_FAILURE != 0 }
    /// Returns whether a WebSocket protocol error occurred.
    #[inline] pub const fn is_protocol_error(self)     -> bool { self.0 & action::PROTOCOL_ERROR != 0 }
    /// Returns whether a WebSocket ping was received.
    #[inline] pub const fn is_ping(self)               -> bool { self.0 & action::PING != 0 }
    /// Returns whether a WebSocket pong was received.
    #[inline] pub const fn is_pong(self)               -> bool { self.0 & action::PONG != 0 }
    /// Returns whether an empty WebSocket data frame was received.
    #[inline] pub const fn is_empty_frame(self)        -> bool { self.0 & action::EMPTY_FRAME != 0 }

    /// Returns whether the graceful flag is set.
    #[inline] pub const fn is_graceful(self)           -> bool { self.0 & mask::GRACEFUL != 0 }
    /// Returns whether the abrupt flag is set.
    #[inline] pub const fn is_abrupt(self)             -> bool { self.0 & mask::ABRUPT   != 0 }
    /// Returns whether the remote-origin flag is set.
    #[inline] pub const fn is_remote(self)             -> bool { self.0 & mask::REMOTE   != 0 }
    /// Returns whether the event is local: the remote-origin flag is absent. Timeouts count as
    /// local even without an explicit local-origin flag.
    #[inline] pub const fn is_this(self)               -> bool { self.0 & mask::REMOTE   == 0 } // An absent remote flag also covers locally detected timeouts.
    /// Returns whether the event occurred on the receive path.
    #[inline] pub const fn is_on_receive(self)         -> bool { self.0 & mask::RECEIVE  != 0 }
    /// Returns whether the event occurred on the transmit path.
    #[inline] pub const fn is_on_transmit(self)        -> bool { self.0 & mask::TRANSMIT != 0 }
    /// Returns whether the event belongs to the WebSocket layer.
    #[inline] pub const fn is_websocket(self)          -> bool { self.0 & mask::WEBSOCKET != 0 }

    /// Returns whether both the close action and graceful flag are set. A WebSocket closing
    /// handshake uses the closing action and does not satisfy this predicate.
    #[inline] pub const fn is_close_graceful(self) -> bool {
        self.0 & action::CLOSE != 0 && (self.0 & mask::GRACEFUL != 0)
    }

    /// Returns a static description for a named event, or a fallback for unrecognized combinations.
    pub fn description(self) -> &'static str {
        match self {
            Self::REMOTE_CONNECT                  => "Remote connection accepted",
            Self::THIS_CONNECT                    => "Local connection established",
            Self::REMOTE_CLOSE_GRACEFUL           => "Remote graceful close",
            Self::THIS_CLOSE_GRACEFUL             => "Local graceful close",
            Self::REMOTE_CLOSE_ABRUPTLY           => "Remote abrupt close",
            Self::THIS_CLOSE_ABRUPTLY             => "Local abrupt close",
            Self::TRANSMIT_TIMEOUT                => "Transmit timeout",
            Self::RECEIVE_TIMEOUT                 => "Receive timeout",
            Self::WEBSOCKET_THIS_CONNECT          => "WebSocket local connection established",
            Self::WEBSOCKET_REMOTE_CONNECTED      => "WebSocket remote connected",
            Self::WEBSOCKET_HANDSHAKE_FAILURE     => "WebSocket handshake failure",
            Self::WEBSOCKET_PROTOCOL_ERROR        => "WebSocket protocol error",
            Self::WEBSOCKET_PING                  => "WebSocket ping",
            Self::WEBSOCKET_PONG                  => "WebSocket pong",
            Self::WEBSOCKET_EMPTY_FRAME           => "WebSocket empty frame",
            Self::WEBSOCKET_REMOTE_CLOSE_GRACEFUL => "WebSocket remote graceful close",
            Self::WEBSOCKET_THIS_CLOSE_GRACEFUL   => "WebSocket local graceful close",
            Self::WEBSOCKET_REMOTE_CLOSE_ABRUPTLY => "WebSocket remote abrupt close",
            Self::WEBSOCKET_THIS_CLOSE_ABRUPTLY   => "WebSocket local abrupt close",
            Self::WEBSOCKET_TRANSMIT_TIMEOUT      => "WebSocket transmit timeout",
            Self::WEBSOCKET_RECEIVE_TIMEOUT       => "WebSocket receive timeout",
            _                                     => "Unknown event",
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.description())
    }
}

/// What an event did, in words: a timeout first, then the first of its action bits - an event may
/// carry several.
pub fn action_description(event_code: u32) -> &'static str {
    const ORDER: [(u32, &str); 10] = [
        (mask::TIMEOUT, "Closed by timeout"),
        (action::CONNECT, "Connection established"),
        (action::CLOSE, "Connection terminated"),
        (action::READY, "Upgrade done"),
        (action::CLOSING, "Closing handshake"),
        (action::PING, "Ping received"),
        (action::PONG, "Pong received"),
        (action::EMPTY_FRAME, "Empty frame received"),
        (action::HANDSHAKE_FAILURE, "Handshake failure"),
        (action::PROTOCOL_ERROR, "Protocol error"),
    ];
    for (bit, text) in ORDER {
        if event_code & bit != 0 {
            return text;
        }
    }
    "Unknown action"
}

/// Handlers of the events ready to use: functions, nothing allocated to hold them.
pub mod utils {
    use super::Event;

    /// Every event to the console, in words: the connection up to its first `:`, what happened, the
    /// rest of it. The stack too, when `RUST_BACKTRACE` asks for it.
    pub fn print_console(connection: &dyn std::fmt::Display, event: Event) {
        println!("{}", describe(connection, event));
        let b = std::backtrace::Backtrace::capture();
        if b.status() == std::backtrace::BacktraceStatus::Captured {
            println!("debugging stack of on_event\n{b}");
        }
    }

    /// The line [`print_console`] prints.
    pub fn describe(connection: &dyn std::fmt::Display, event: Event) -> String {
        let text = connection.to_string();
        let (at, to) = text.split_once(':').unwrap_or((text.as_str(), ""));
        let ws = if event.is_websocket() { "WebSocket " } else { "" };
        let remote = event.is_remote();
        let what: std::borrow::Cow<'static, str> = if event.is_connect() {
            (if remote { "Accepted connection from " } else { "Connected to " }).into()
        } else if event.is_timeout() {
            (if event.is_on_transmit() { "Closed by timeout while transmitting to " } else { "Closed by timeout while receiving from " }).into()
        } else if event.is_close() {
            (match (remote, event.is_graceful()) {
                (true, true) => "Remote peer gracefully closed connection with ",
                (true, false) => "Connection abruptly closed by remote peer with ",
                (false, true) => "Gracefully closed connection to ",
                (false, false) => "Abruptly closed connection to ",
            })
            .into()
        } else if event.is_ready() {
            format!("{ws}{}", if remote { "upgrade done for " } else { "upgrade done by " }).into()
        } else if event.is_closing() {
            format!("{ws}{}", if remote { "closing handshake started by " } else { "closing handshake answered by " }).into()
        } else if event.is_ping() {
            "PING received from ".into()
        } else if event.is_pong() {
            "PONG received from ".into()
        } else if event.is_empty_frame() {
            "Empty WebSocket data frame received from ".into()
        } else if event.is_handshake_failure() {
            format!("{ws}handshake failed with ").into()
        } else if event.is_protocol_error() {
            format!("{ws}protocol broken by ").into()
        } else {
            format!("Unknown event ({}) from ", event.raw()).into()
        };
        format!("{at}:{what}{to}")
    }

    /// Does nothing: a handler where one is needed and nothing is to be done.
    #[inline]
    pub fn stub(_connection: &dyn std::fmt::Display, _event: Event) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_match_csharp_semantics() {
        let e = Event::REMOTE_CLOSE_GRACEFUL;
        assert!(e.is_close());
        assert!(e.is_graceful());
        assert!(e.is_remote());
        assert!(!e.is_abrupt());
        assert!(!e.is_websocket());
        assert!(e.is_close_graceful());

        let ws = Event::WEBSOCKET_PING;
        assert!(ws.is_websocket());
        assert!(ws.is_remote());
        assert!(ws.is_ping());
    }

    /// Formats the event and connection context, splitting the connection text at its first colon.
    #[test]
    fn print_console_says_it_as_java() {
        let c = "[3] host 127.0.0.1:5000:127.0.0.1:6000";
        assert_eq!(utils::describe(&c, Event::REMOTE_CONNECT), "[3] host 127.0.0.1:Accepted connection from 5000:127.0.0.1:6000");
        assert_eq!(utils::describe(&c, Event::REMOTE_CLOSE_GRACEFUL), "[3] host 127.0.0.1:Remote peer gracefully closed connection with 5000:127.0.0.1:6000");
        assert_eq!(utils::describe(&c, Event::RECEIVE_TIMEOUT), "[3] host 127.0.0.1:Closed by timeout while receiving from 5000:127.0.0.1:6000");
        assert_eq!(utils::describe(&"[1] h : closed", Event::WEBSOCKET_PING), "[1] h :PING received from  closed");
        utils::stub(&c, Event::REMOTE_CONNECT);
    }

    #[test]
    fn timeouts_are_close_events_as_in_csharp() {
        for to in [
            Event::RECEIVE_TIMEOUT,
            Event::TRANSMIT_TIMEOUT,
            Event::WEBSOCKET_RECEIVE_TIMEOUT,
            Event::WEBSOCKET_TRANSMIT_TIMEOUT,
        ] {
            assert!(to.is_close(), "{to:?}: C# IsClose() is true for timeouts");
            assert!(to.is_timeout(), "{to:?}: C# IsTimeout() is true");
        }
        assert!(Event::RECEIVE_TIMEOUT.is_on_receive());
        assert!(!Event::RECEIVE_TIMEOUT.is_on_transmit());
        assert!(Event::TRANSMIT_TIMEOUT.is_on_transmit());
    }

    #[test]
    fn websocket_connect_is_ready_not_connect() {
        for c in [Event::WEBSOCKET_THIS_CONNECT, Event::WEBSOCKET_REMOTE_CONNECTED] {
            assert!(c.is_ready(), "{c:?}");
            assert!(!c.is_connect(), "{c:?}: C# connect events are READY");
        }
        assert_eq!(
            Event::WEBSOCKET_REMOTE_CONNECT,
            Event::WEBSOCKET_REMOTE_CONNECTED,
            "the Java runtime's name for the same value"
        );
    }

    #[test]
    fn websocket_graceful_close_is_closing_not_close() {
        for c in [
            Event::WEBSOCKET_REMOTE_CLOSE_GRACEFUL,
            Event::WEBSOCKET_THIS_CLOSE_GRACEFUL,
        ] {
            assert!(c.is_closing(), "{c:?}");
            assert!(c.is_graceful(), "{c:?}");
            assert!(!c.is_close(), "{c:?}: C# IsClose() is false for a closing handshake");
            assert!(!c.is_close_graceful(), "{c:?}");
        }
        // TCP graceful closes stay CLOSE.
        assert!(Event::THIS_CLOSE_GRACEFUL.is_close_graceful());
    }

    #[test]
    fn description_matches_csharp_strings() {
        assert_eq!(
            Event::REMOTE_CLOSE_GRACEFUL.description(),
            "Remote graceful close"
        );
        assert_eq!(
            Event::WEBSOCKET_HANDSHAKE_FAILURE.description(),
            "WebSocket handshake failure"
        );
        assert_eq!(
            Event::WEBSOCKET_TRANSMIT_TIMEOUT.description(),
            "WebSocket transmit timeout"
        );
        assert_eq!(Event(0xDEAD_BEEF).description(), "Unknown event");
        // Display delegates to description.
        assert_eq!(format!("{}", Event::THIS_CONNECT), "Local connection established");
    }

    #[test]
    fn action_description_roundtrip() {
        assert_eq!(action_description(Event::REMOTE_CONNECT.0), "Connection established");
        assert_eq!(action_description(Event::THIS_CLOSE_ABRUPTLY.0), "Connection terminated");
        assert_eq!(action_description(Event::TRANSMIT_TIMEOUT.0), "Closed by timeout");
        assert_eq!(action_description(Event::WEBSOCKET_PING.0), "Ping received");
        assert_eq!(action_description(Event::WEBSOCKET_THIS_CONNECT.0), "Upgrade done");
        assert_eq!(action_description(Event::WEBSOCKET_THIS_CLOSE_GRACEFUL.0), "Closing handshake");
        assert_eq!(action_description(0), "Unknown action");
    }

    #[test]
    fn distinct_events_have_distinct_bits() {
        let all = [
            Event::REMOTE_CONNECT,
            Event::THIS_CONNECT,
            Event::REMOTE_CLOSE_GRACEFUL,
            Event::THIS_CLOSE_GRACEFUL,
            Event::REMOTE_CLOSE_ABRUPTLY,
            Event::THIS_CLOSE_ABRUPTLY,
            Event::TRANSMIT_TIMEOUT,
            Event::RECEIVE_TIMEOUT,
            Event::WEBSOCKET_THIS_CONNECT,
            Event::WEBSOCKET_REMOTE_CONNECTED,
            Event::WEBSOCKET_HANDSHAKE_FAILURE,
            Event::WEBSOCKET_PROTOCOL_ERROR,
            Event::WEBSOCKET_PING,
            Event::WEBSOCKET_PONG,
            Event::WEBSOCKET_EMPTY_FRAME,
            Event::WEBSOCKET_REMOTE_CLOSE_GRACEFUL,
            Event::WEBSOCKET_THIS_CLOSE_GRACEFUL,
            Event::WEBSOCKET_REMOTE_CLOSE_ABRUPTLY,
            Event::WEBSOCKET_THIS_CLOSE_ABRUPTLY,
            Event::WEBSOCKET_TRANSMIT_TIMEOUT,
            Event::WEBSOCKET_RECEIVE_TIMEOUT,
        ];
        let mut seen = std::collections::HashSet::new();
        for e in all {
            assert!(seen.insert(e.0), "duplicate Event encoding: {:?}", e);
        }
    }
}
