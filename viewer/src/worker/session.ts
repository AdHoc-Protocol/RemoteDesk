// The worker of the session, where the connection to the machine runs: the WebSocket under Noise XX, the packs, the login by the
// token, the video decoded and drawn on the canvas the page gave away, the sound, the files, the clipboard. Apart from the page:
// what the input and the rest of the page do on the main thread does not hold the frames back, nor a big frame the input. The
// page talks to it by the messages of ../messages.ts; built separately and started by the page from a Blob URL.
//
// ../../protocol/ is the code AdHoc generates from D:\rdp\RemoteDesk.cs ( deployed there by D:\rdp\generated\RemoteDesk.md ).

import AdHoc from "../../protocol/lib/AdHoc";
import Crypto from "../../protocol/lib/Crypto";
import Network from "../../protocol/lib/Network.browser";
import Viewer from "../../protocol/gen/Viewer";
import Session from "../../protocol/gen/Session/Connection";
import Actor0 from "../../protocol/gen/Session/Actor0";
import { noise } from "../keys";
import { M, W, Admin, Elevation, Refusal, type ToWorker } from "../messages";
import { toPage, listen } from "./page";
import * as video from "./video";
import * as sound from "./sound";
import * as files from "./files";
import * as clipboard from "./clipboard";
import * as input from "./input";

let client: Network.WebSocketClient<Session> | undefined;
/** Way 4, direct: the port the bytes of the data channel come and go by, instead of a WebSocket. */
let port: MessagePort | undefined;

/** Way 4, direct: one session over the data channel the page met the machine on ( ../direct.ts ): the page keeps the channel and
 *  passes its bytes over a port - each message its buffer, moved; `undefined` - closed. The port is the socket of the session,
 *  as the WebRTC connection of the runtime makes one of a channel; this end writes the first message of Noise. */
class Direct extends Network.TCP {
    constructor(onFailure: (source: unknown, error: Error) => void) {
        super("viewer direct", host => new Network.TCP.ExternalConnection(host), onFailure, 1 << 16);
    }

    async open(on: MessagePort): Promise<Session> {
        const c = this.allocate();
        const session = new Session();
        const declared = c.seat(session);
        if (declared !== undefined && declared.codec === undefined) throw Network.TCP.ExternalConnection.refused(); // no keys for it
        if (declared?.ready !== undefined) await declared.ready(); // compiled asynchronously
        c.declared = declared;
        let open = true;
        on.onmessage = e => {
            if (e.data !== undefined) return c.receive(e.data as ArrayBuffer);
            open = false;
            if (c.isActive()) c.abort(); // the channel closed: the session is lost
        };
        c.transmitterConnected({
            get isOpen() { return open; },
            send: (bytes: Uint8Array<ArrayBuffer>) => { const b = bytes.slice().buffer; on.postMessage(b, [b]); }, // its buffer is the transmitter's: a copy goes
            close: () => { if (open) on.postMessage(undefined); open = false; },
            subscribe: () => () => { },
        }, "direct");
        return session;
    }
}
/** The connection logged in, or logging in: the events of others are not the session's. */
let connection: Session | undefined;
/** The canvas of the page for the next session: given with the connect, drawn on once the machine welcomes. */
let canvas: OffscreenCanvas | undefined;
/** This viewer controls the machine: else what the page sends of input, fit, microphone, clipboard and files to the machine is
 *  dropped here - the machine would not take it. */
let yours = false;

/** What runs while the machine has the page in a session. */
let live: {
    conn: Session;
    video: video.Video;
    sound: sound.Audio;
    files: files.Files;
    clipboard: clipboard.Clipboard;
} | undefined;

function startSession(conn: Session): void {
    const surface = canvas ?? new OffscreenCanvas(1, 1); // a canvas is given once: a second welcome draws where nobody sees
    canvas = undefined;
    live = { conn, video: video.start(conn, surface), sound: sound.start(conn), files: files.start(conn), clipboard: clipboard.start(conn) };
}

function stopSession(): void {
    yours = false;
    if (!live) return;
    live.clipboard.stop();
    live.files.stop();
    live.sound.stop();
    live.video.stop();
    live = undefined;
}

const same = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((v, i) => v === b[i]);

/** The letters of the token and of the check, 5 bits each: as the program has them ( host/src/session.rs ). */
const ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const hashed = new Uint8Array(32);

/**
 * The check of `key` the line of the machine carries after its token: 8 letters, the first 40 bits of its SHA-256 - as `key_check` of
 * the program makes it. Here at once, in the handshake: the token goes only to the machine of the line.
 */
function keyCheck(key: Uint8Array): string {
    Crypto.Sha256.hash(hashed, key);
    let bits = 0n;
    for (let i = 0; i < 5; i++) bits = bits << 8n | BigInt(hashed[i]!);
    let out = "";
    for (let i = 7; i >= 0; i--) out += ALPHABET[Number(bits >> BigInt(i * 5) & 31n)];
    return out;
}

async function connect(m: Extract<ToWorker, { t: W.CONNECT }>): Promise<void> {
    client?.closeGracefully(1000, "a new connection");
    client = undefined;
    port?.postMessage(undefined);
    port = undefined;
    connection = undefined;
    stopSession();
    canvas = m.canvas;
    input.origin(m.origin);

    const secret = m.secret, kept = m.kept, check = m.check;
    noise.secret = () => secret.slice(); // a copy each time: what the handshake takes it may wipe
    // the region "The key the far end proved" asks: the line of the machine carries a check of its key - it must fit, whatever was kept;
    // a line without one - the page keeps the first key it saw at an address, a later one must be the same. No: the handshake ends
    // here, and the token, sent after it, goes nowhere
    noise.proved = key => {
        const checked = check !== undefined;
        const accepted = checked ? keyCheck(key) === check : kept === undefined || same(kept, key);
        toPage({ t: M.PROVED, key: key.slice(), accepted, checked });
        return accepted;
    };

    if (m.port !== undefined) return direct(m, m.port);

    const dialing = client = new Network.WebSocketClient<Session>("viewer", () => new Session(),
        (_source, error) => toPage({ t: M.STATUS, error: error.message }), 1 << 16);
    dialing.maxRetries = 0;

    let error = "";
    const conn = await dialing.connect(m.address, 5000).catch((e: Error) => { error = e.message; return undefined; });
    if (dialing !== client) return void (conn && dialing.abort()); // the page left, or dials again meanwhile
    if (!conn) return toPage({ t: M.DIALED, ok: false, error });
    connection = conn;

    const hello = new Viewer.Viewer_.Hello();
    hello.token = m.token;
    hello.client_name = m.name;
    Actor0.State.O.transmitter.send_Viewer_Hello(hello, conn);
    toPage({ t: M.DIALED, ok: true, error: "" });
}

/** Way 4, direct: the session over the data channel of the page. */
async function direct(m: Extract<ToWorker, { t: W.CONNECT }>, on: MessagePort): Promise<void> {
    port = on;
    const host = new Direct((_source, error) => toPage({ t: M.STATUS, error: error.message }));
    let error = "";
    const conn = await host.open(on).catch((e: Error) => { error = e.message; return undefined; });
    if (port !== on) return; // the page left, or connects again meanwhile
    if (!conn) return toPage({ t: M.DIALED, ok: false, error });
    connection = conn;
    const hello = new Viewer.Viewer_.Hello();
    hello.token = m.token;
    hello.client_name = m.name;
    Actor0.State.O.transmitter.send_Viewer_Hello(hello, conn);
    toPage({ t: M.DIALED, ok: true, error: "" });
}

function leave(): void {
    client?.closeGracefully(1000, "the viewer leaves");
    client = undefined;
    port?.postMessage(undefined);
    port = undefined;
    connection = undefined;
    canvas = undefined;
    stopSession();
}

// ── the login ──

const reasons: Record<number, Refusal> = {
    [Viewer.Reason.UNEXPECTED]: Refusal.UNEXPECTED,
    [Viewer.Reason.AUTHENTICATION_FAILED]: Refusal.WRONG_TOKEN,
    [Viewer.Reason.HOST_OVERLOAD]: Refusal.OVERLOAD,
    [Viewer.Reason.FULL]: Refusal.FULL,
};

Actor0.State.Granting.OnReceived.Machine_Welcome.handlers.subscribe((welcome, conn) => {
    if (conn !== connection) return;
    stopSession();
    startSession(conn);
    toPage({ t: M.WELCOME, host: welcome.host_name, administrator: welcome.administrator });
});

Actor0.State.Granting.OnReceived.Machine_Denied.handlers.subscribe((denied, conn) => {
    if (conn !== connection) return;
    const reason = Viewer.Machine_.Denied.get.reason(denied);
    const left = Viewer.Machine_.Denied.hasValue.attempts_left(denied) ? Viewer.Machine_.Denied.get.attempts_left(denied) : -1;
    toPage({ t: M.DENIED, reason: reasons[reason] ?? Refusal.OTHER, code: reason, left, forget: reason === Viewer.Reason.AUTHENTICATION_FAILED });
});

// ── the rights of an administrator on the machine: asked by the operator, confirmed by the user of the machine ──

Actor0.State.Working.OnReceived.Machine_Elevation.handlers.subscribe((elevation, conn) => {
    if (conn !== connection) return;
    const notice = (elevation: Elevation) => toPage({ t: M.NOTICE, elevation });
    switch (Viewer.Machine_.Elevation.get.state(elevation)) {
        case Viewer.Machine_.Elevation.State.ASKING: notice(Elevation.ASKING); break;
        case Viewer.Machine_.Elevation.State.CONFIRMED: notice(Elevation.CONFIRMED); break;
        case Viewer.Machine_.Elevation.State.DECLINED: notice(Elevation.DECLINED); break;
        case Viewer.Machine_.Elevation.State.ALREADY: notice(Elevation.NONE); toPage({ t: M.ADMIN, admin: Admin.YES }); break;
        case Viewer.Machine_.Elevation.State.NOT_NEEDED: notice(Elevation.NONE); toPage({ t: M.ADMIN, admin: Admin.NOT_NEEDED }); break;
        default: notice(Elevation.FAILED); break;
    }
});

// ── who watches and who controls: every viewer sees, one controls ──

Actor0.State.Working.OnReceived.Machine_Audience.handlers.subscribe((audience, conn) => {
    if (conn !== connection || !live) return;
    const was = yours;
    yours = audience.yours;
    if (was && !yours) {
        live.sound.micOff(false); // the machine stopped it
        live.video.control(false);
    } else if (yours) live.video.control(true);
    toPage({ t: M.AUDIENCE, watchers: audience.watchers, yours, controller: audience.controller ?? "" });
});

// ── the end of the session ──

Session.OnEvent.handlers.subscribe((conn, event) => {
    if (conn !== connection || !AdHoc.Connection.External.Event.Action.isClose(event)) return;
    connection = undefined;
    stopSession();
    toPage({ t: M.CLOSED });
});

// ── the page ──

listen(m => {
    switch (m.t) {
        case W.CONNECT: return void connect(m);
        case W.LEAVE: return leave();
        case W.ELEVATE: if (connection && yours) Actor0.State.Working.ONE.transmitter.send_Viewer_ElevationRequest(connection); return;
        case W.PLAYER: return live ? live.sound.player(m.port) : m.port.close();
        case W.MIC: return live && yours ? live.sound.mic(m.port) : m.port.close();
    }
    if (!live) return; // the session ended: what the page sent for it before it heard so is dropped
    switch (m.t) {
        case W.POINTER:
        case W.STAMP:
        case W.KEY:
        case W.TEXT:
        case W.BUTTON:
        case W.TOUCH: return void (yours && input.apply(m, live.conn, live.video));
        case W.VISIBLE: return live.video.visible(m.on);
        case W.FPS: return live.video.framerate(m.fps);
        case W.FIT: return void (yours && live.video.fit(m.on, m.now, m.w, m.h));
        case W.SELECT: return live.video.select(m.screen);
        case W.SOUND: return live.sound.sound(m.on);
        case W.MIC_OFF: return live.sound.micOff(m.tell && yours);
        case W.CLIPBOARD: return void (yours && live.clipboard.offer(m.text));
        case W.FILES: return void (yours && live.files.send(m.files));
        case W.FILE_REQUEST: return live.files.request();
        case W.FILE_CANCEL: return live.files.cancel();
        case W.TAKE_CONTROL: return void Actor0.State.Working.ONE.transmitter.send_Viewer_TakeControl(live.conn);
        case W.MARK: return input.mark(m, live.conn, live.video);
    }
});
