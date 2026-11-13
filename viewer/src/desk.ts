// The desk of RemoteDesk: the bar with its buttons and the screen of a machine, built into any element of any page, and the session
// it shows. The session itself - the WebSocket under Noise XX to the machine, the login by the token, the packs, the video decoded and
// drawn - runs in a worker ( ./worker/session.ts, built apart and embedded here as text: a page opened from disk cannot start a worker
// from a file; ../worker-plugin.mjs builds it, for this project and for one that imports this module ). The page keeps the rest: what
// it keeps on the device ( a `Store` the page gives ), the input, the clipboard, the sound devices, the files picked and saved; it talks
// to the worker by the messages of ./messages.ts.
//
// The viewer page ( ./app.ts ) puts its login form before it; DeFreez opens it over its own page. A page may hold any number of desks at
// once, each with its own worker, connection, token, sound and keys: nothing of a desk is kept outside it, no element is found by an id.
// The keys and the clipboard go to the desk in use ( the focus is in it ); a desk hidden ( `show( false )` ) takes no video, as a page
// not seen. Its style is scoped by the class prefix `rd-` and injected once a page; the page may set the `--rd-*` colours on `.rd-desk`.
//
// Several viewers may watch one machine at once: each sees the screen, one controls it. The bar tells how many watch and who controls;
// a desk that does not control sends no input, fit, microphone, clipboard or files to the machine - its button takes the control.

/// <reference path="./session-worker.d.ts" />
import sessionCode from "session-worker";
import { relay } from "./direct";
import { M, W, Admin, Elevation, FileLine, FileWhy, Refusal, type ToMain, type ToWorker } from "./messages";
import * as screen from "./screen";
import * as input from "./input";
import * as clipboard from "./clipboard";
import * as files from "./files";
import * as audio from "./audio";

/** What the desk keeps on the device: the secret key of Noise of this page ( "secret" ), the key each machine proved ( "host.<address>" ), the most frames a second ( "fps" ). */
export interface Store {
    get(name: string): string | undefined;
    /** `undefined` - drop it. */
    set(name: string, value: string | undefined): void;
}

/** The words of the desk; `{name}` in one is replaced by its value. A page gives its own for its language. */
export const english = {
    connecting: "Connecting…",
    loggingIn: "Logging in…",
    cannotConnect: "Cannot connect to {address}.",
    dialFailed: "Cannot connect: {error}",
    linkFailed: "The connection failed: {error}",
    sessionFailed: "The session failed: {error}",
    badAddress: "The address of the machine starts with ws:// or wss://.",
    noToken: "The token of the machine is needed.",
    lost: "The connection is lost: connecting again in {seconds} s ( attempt {attempt} of {of} )…",
    directLost: "The direct connection is lost: make a new one.",
    noAnswer: "The machine does not answer. Connect again when it is back.",
    closed: "The connection is closed.",
    left: "Disconnected.",
    refusedUnexpected: "Refused: an unexpected error on the machine.",
    refusedToken: "Refused: a wrong token.",
    refusedOverload: "Refused: too many wrong tokens - the machine takes none for a minute.",
    refusedFull: "Refused: the machine has as many viewers as it takes.",
    refusedOther: "Refused: reason {code}.",
    attemptsLeft: " Attempts left: {n}.",
    keyKept: "The key of this machine, kept from now on: {key}",
    keyIs: "The key of the machine: {key}",
    keyOther: "The machine answers with another key: {key}. The kept one: {kept}.",
    keyForgotten: "The key is forgotten: the next login keeps the one the machine shows then.",
    machine: "the machine",
    display: "Display {n}",
    displayTitle: "The display to show",
    inUse: "in use",
    stats: "{w}×{h} · {fps} fps · {mbps} Mbit/s · capture {capture} ms · encode {encode} ms",
    latency: " · input→frame {ms} ms",
    soundOn: "Sound: on",
    soundOff: "Sound: off",
    soundTitle: "The sound of the machine",
    micOn: "Microphone: on",
    micOff: "Microphone: off",
    micTitle: "The microphone of this device, played on the machine",
    noSound: "No sound: {error}",
    micNotAllowed: "The microphone: not allowed",
    micError: "The microphone: {error}",
    micFailed: "The machine cannot play the microphone: its sound device failed.",
    send: "Send files",
    sendTitle: "Files to the Downloads folder of the machine; or drop them on the screen",
    get: "Get a file",
    getTitle: "The user of the machine picks a file there",
    cancel: "Cancel",
    cancelTitle: "Stop the transfer of the file",
    adminAsk: "Ask for admin rights",
    adminAskTitle: "The user of the machine is asked to confirm in the prompt of UAC; then the program of the machine starts again as administrator",
    adminYes: "Administrator",
    adminYesTitle: "The program of the machine runs as administrator: windows of programs run as administrator take the input too",
    adminNotNeeded: "No admin rights needed",
    adminNotNeededTitle: "The system of the machine lets the input reach every window as it is",
    elevationAsking: "The user of the machine is asked to confirm the rights of an administrator…",
    elevationConfirmed: "Confirmed: the machine starts again as administrator, the page connects again.",
    elevationDeclined: "The user of the machine declined the rights of an administrator.",
    elevationFailed: "The machine could not ask for the rights of an administrator.",
    fitOn: "Fit: on",
    fitOff: "Fit: off",
    fitTitle: "The resolution of the machine follows the size of this window; off restores it",
    fps: "{n} fps",
    fpsTitle: "The most frames a second the machine sends: 60 is smoother, and takes more of the link and of the machine",
    lock: "Lock pointer",
    lockTitle: "Mouse moves go to the machine as relative moves; Esc frees the pointer",
    full: "Full screen",
    fullTitle: "Full screen: the keys of the system go to the machine too",
    leave: "Disconnect",
    watching: "{n} watching",
    controls: "controls: {name}",
    youControl: "you control",
    aViewer: "a viewer",
    takeControl: "Take control",
    takeControlTitle: "Your keys, mouse, microphone, clipboard and files go to the machine; the viewer that controls it now only watches",
    fileProgress: "{name}: {n}%",
    fileMore: " · {n} more",
    fileSaved: "{name}: saved in the Downloads of the machine",
    fileReceived: "{name}: {n} bytes received",
    fileFailed: "{name}: {why}",
    fileCode: " ( error {code} )",
    fileWait: "Wait: a file is on its way.",
    filePicking: "The user of the machine picks the file…",
    fileCancelled: "Cancelled.",
    aFile: "the file of the machine",
    whyCancelled: "cancelled",
    whyProtocol: "the sides did not understand each other",
    whyPermission: "not allowed",
    whyDisk: "no space left on the disk",
    whyIo: "a read or write error",
    whyOther: "failed",
    whyClosed: "the connection is closed",
    whyEnded: "the session ended",
};

export type Texts = Record<keyof typeof english, string>;

/** `template` with `{name}` replaced by `vars.name`. */
export const fill = (template: string, vars: Record<string, string | number>): string =>
    template.replace(/\{(\w+)\}/g, (all, name: string) => name in vars ? String(vars[name]) : all);

export interface Options {
    /** `ws://…` or `wss://…`: where the machine listens; with `channel` - only what the bar names. */
    address: string;
    /** Way 4, direct: the data channel the page and the machine met on ( ./direct.ts ) - the session goes over it, given to the
     *  worker. Lost, it is not dialed again: a new one takes a new exchange of the lines. */
    channel?: RTCDataChannel | undefined;
    token: string;
    /** The name the machine shows its user: who connects. */
    name: string;
    /** What the bar names before the machine tells its own name: the address when not given. */
    title?: string | undefined;
    store: Store;
    /** The words of the page; those not given - English. */
    texts?: Partial<Texts> | undefined;
    /** How the connection goes: `bad` - it failed, or is lost. */
    onStatus?: ((text: string, bad: boolean) => void) | undefined;
    /** The key the machine proved, in words; `other` - not the key kept for the address: `forgetKey` lets the next login keep the new one. */
    onKey?: ((text: string, other: boolean) => void) | undefined;
    /** The machine let the page in ( `on` ): `host` - its name, "" - not told; or the session broke and is dialed again. */
    onLive?: ((on: boolean, host: string) => void) | undefined;
    /** The desk is closed, its element gone: `bad` - not left by the user; `wrongToken` - the machine took the token for a wrong one. */
    onEnded?: ((text: string, bad: boolean, wrongToken: boolean) => void) | undefined;
    /** Where the connection is: what a page draws of it. */
    onStage?: ((stage: Stage) => void) | undefined;
}

/**
 * Where the connection is, step by step: `dialing` - the page reaches for the machine; `reached` - the machine answered, the
 * handshake goes; `proved` - its key is proved, the token goes; `otherKey` - the machine answers with a key not kept for it;
 * `live` - the machine let the page in; `unreachable` - nobody answered at the address; `refused` - the machine did not take
 * the token, or has no room; `closed` - it closed while the page logged in; `failed` - the connection failed under way; `lost` - a
 * live session broke, dialed again; `lostDirect` - the direct one broke; `noAnswer` - dialed again, in vain; `left` - the user left.
 */
export type Stage = "dialing" | "reached" | "proved" | "otherKey" | "live" | "unreachable" | "refused" | "closed" | "failed" | "lost"
    | "lostDirect" | "noAnswer" | "left";

export interface Desk {
    /** The element of the desk, in the container. */
    readonly root: HTMLElement;
    /** Leaves the machine: the desk closes. */
    leave(): void;
    /** Shows the desk, or hides it: a hidden one takes no video; a shown one takes the keys. */
    show(on: boolean): void;
}

/** The line a machine prints for its viewer - `ws://<router>:<port>#<token>` -: its address and token; `undefined` - not such a line. */
export function splitLine(line: string): { address: string; token: string } | undefined {
    const text = line.trim(), at = text.indexOf("#");
    return at < 0 ? undefined : { address: text.slice(0, at), token: text.slice(at + 1) };
}

/** Forgets the key kept for `address`: the next login keeps the one the machine shows then. */
export function forgetKey(store: Store, address: string): void {
    store.set("host." + address.trim(), undefined);
}

const hex = (bytes: Uint8Array) => Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");
const unhex = (text: string) => Uint8Array.from({ length: text.length / 2 }, (_, i) => parseInt(text.slice(i * 2, i * 2 + 2), 16));
const fingerprint = (key: Uint8Array) => hex(key).replace(/(.{4})(?!$)/g, "$1 ");

// ── the style, once a page ──

const STYLE = `
.rd-desk{--rd-bg:#f6f7f9;--rd-fg:#1d2330;--rd-muted:#5b6475;--rd-line:#d5d9e0;--rd-accent:#2f6fde;--rd-bad:#c4352b;display:flex;flex-direction:column;height:100%;min-height:0;box-sizing:border-box;background:var(--rd-bg);color:var(--rd-fg);font:14px/1.45 system-ui,sans-serif}
@media (prefers-color-scheme:dark){.rd-desk{--rd-bg:#15181e;--rd-fg:#e4e7ec;--rd-muted:#98a0ae;--rd-line:#2c323c;--rd-accent:#6b9bf2;--rd-bad:#ef6a5f}}
.rd-desk *{box-sizing:border-box}
.rd-bar{display:flex;flex-wrap:wrap;gap:4px 12px;align-items:center;padding:6px 12px;border-bottom:1px solid var(--rd-line);color:var(--rd-muted)}
.rd-bar button{margin:0;padding:0;border:0;background:transparent;color:var(--rd-accent);font:inherit;cursor:pointer;white-space:nowrap}
.rd-bar button:disabled{color:var(--rd-muted);cursor:default}
.rd-bar select{padding:2px 6px;border:1px solid var(--rd-line);border-radius:4px;background:transparent;color:inherit;font:inherit}
.rd-grow{flex:1}
.rd-stats{min-width:0;font:12px ui-monospace,monospace;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.rd-notice{color:var(--rd-bad)}
.rd-transfer{max-width:40ch;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.rd-audience{max-width:40ch;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.rd-screen{flex:1;min-height:0;display:flex;align-items:center;justify-content:center;background:#000}
.rd-screen.rd-drop{outline:3px dashed var(--rd-accent);outline-offset:-3px}
.rd-screen canvas{max-width:100%;max-height:100%;outline:none}
.rd-state{flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:8px;padding:24px;text-align:center}
.rd-state .rd-bad{color:var(--rd-bad)}
.rd-key{max-width:60ch;color:var(--rd-muted);font:12px/1.4 ui-monospace,monospace;word-break:break-all}
.rd-desk:not(.rd-on) .rd-s,.rd-desk:not(.rd-on) .rd-screen,.rd-desk.rd-on .rd-state,.rd-desk.rd-on .rd-gap{display:none}
`;

/** The style of the desks, once a page: the page itself tells whether it has it. */
function style(): void {
    if (document.head.querySelector("style[data-rd]")) return;
    const element = document.createElement("style");
    element.dataset["rd"] = "";
    element.textContent = STYLE;
    document.head.appendChild(element);
}

const RETRIES = 10;
/** The most frames a second: 30, or 60 - kept on the device. */
const DEFAULT_FPS = 30;

/** Builds the desk into `container` and connects to the machine `options.address`. */
export function open(container: HTMLElement, options: Options): Desk {
    style();
    const t: Texts = options.texts === undefined ? english : { ...english, ...options.texts };
    const store = options.store;
    const address = options.address.trim(), token = options.token;

    // ── the elements ──

    const element = <K extends keyof HTMLElementTagNameMap>(tag: K, className: string, text = "", title = ""): HTMLElementTagNameMap[K] => {
        const e = document.createElement(tag);
        e.className = className;
        if (text) e.textContent = text;
        if (title) e.title = title;
        return e;
    };
    const button = (className: string, text: string, title = "") => {
        const b = element("button", className, text, title);
        b.type = "button";
        return b;
    };

    const root = element("div", "rd-desk");

    // ── the worker of the session: one a desk ──

    const workerUrl = URL.createObjectURL(new Blob([sessionCode, "\n//# sourceURL=viewer-session.js"], { type: "text/javascript" }));
    const worker = new Worker(workerUrl);
    const post = (message: ToWorker, transfer?: Transferable[]) => {
        if (transfer === undefined) worker.postMessage(message);
        else worker.postMessage(message, transfer);
    };
    const bar = element("div", "rd-bar");
    const who = element("span", "rd-who", options.title || address);
    const audienceLine = element("span", "rd-s rd-audience");
    const takeButton = button("rd-s", t.takeControl, t.takeControlTitle);
    takeButton.hidden = true;
    const displaySelect = element("select", "rd-s", "", t.displayTitle);
    displaySelect.hidden = true;
    const stats = element("span", "rd-s rd-stats rd-grow");
    const gap = element("span", "rd-grow rd-gap"); // the place of the stats while there are none
    const transfer = element("span", "rd-s rd-transfer");
    const cancelButton = button("rd-s", t.cancel, t.cancelTitle);
    cancelButton.hidden = true;
    const notice = element("span", "rd-s rd-notice");
    const soundButton = button("rd-s", t.soundOn, t.soundTitle);
    const micButton = button("rd-s", t.micOff, t.micTitle);
    const sendButton = button("rd-s", t.send, t.sendTitle);
    const getButton = button("rd-s", t.get, t.getTitle);
    const pick = element("input", "");
    pick.type = "file";
    pick.multiple = true;
    pick.hidden = true;
    const adminButton = button("rd-s", t.adminAsk, t.adminAskTitle);
    const fitButton = button("rd-s", "", t.fitTitle);
    const fpsButton = button("rd-s", "", t.fpsTitle);
    const lockButton = button("rd-s", t.lock, t.lockTitle);
    const fullButton = button("rd-s", t.full, t.fullTitle);
    const leaveButton = button("", t.leave);
    bar.append(who, audienceLine, takeButton, displaySelect, stats, gap, transfer, cancelButton, notice, soundButton, micButton, sendButton, getButton, pick, adminButton, fitButton, fpsButton, lockButton, fullButton, leaveButton);
    const screenHost = element("div", "rd-screen");
    const state = element("div", "rd-state");
    const stateLine = element("div", "");
    const keyLine = element("div", "rd-key");
    state.append(stateLine, keyLine);
    root.append(bar, screenHost, state);
    container.appendChild(root);

    // ── how it goes ──

    let bad = false;
    const status = (text: string, isBad: boolean) => {
        bad = isBad;
        stateLine.textContent = text;
        stateLine.className = isBad ? "rd-bad" : "";
        options.onStatus?.(text, isBad);
    };

    const stage = (s: Stage) => options.onStage?.(s);

    const showKey = (text: string, other: boolean) => {
        keyLine.textContent = text;
        options.onKey?.(text, other);
    };

    /** The static key of Noise of this page: made once, kept on the device. */
    const secret = (): Uint8Array => {
        const kept = store.get("secret");
        if (kept && kept.length === 64) return unhex(kept);
        const fresh = crypto.getRandomValues(new Uint8Array(32));
        store.set("secret", hex(fresh));
        return fresh;
    };

    /**
     * The key the machine proved by the handshake: the first one seen at an address is kept, a later one must be the same - else
     * someone else answers at that address, and the page does not go on. The worker compared it with the kept one it was given.
     */
    const proved = (key: Uint8Array, accepted: boolean) => {
        // direct: the machine is the one whose line came by hand - no address to keep its key for
        stage(accepted ? "proved" : "otherKey");
        if (options.channel !== undefined) return showKey(fill(t.keyIs, { key: fingerprint(key) }), false);
        const kept = store.get("host." + address);
        if (kept === undefined && accepted) {
            store.set("host." + address, hex(key));
            showKey(fill(t.keyKept, { key: fingerprint(key) }), false);
        } else if (accepted) showKey(fill(t.keyIs, { key: fingerprint(key) }), false);
        else showKey(fill(t.keyOther, { key: fingerprint(key), kept: fingerprint(unhex(kept ?? "")) }), true);
    };

    // ── the session ──

    let ended = false;
    let live = false;
    /** The desk is shown on the page: else it takes no video. */
    let seen = true;
    /** The resolution of the machine follows the size of the screen of the desk: kept over reconnections. */
    let fitting = false;
    /** The canvas of the connection dialed: its control went to the worker with the connect. */
    let nextCanvas: HTMLCanvasElement | undefined;
    let shown: screen.Screen | undefined;
    let typing: input.Input | undefined;
    let copying: clipboard.Clipboard | undefined;
    let moving: files.Files | undefined;
    let hearing: audio.Audio | undefined;
    /** This viewer controls the machine: the input, the clipboard, the fit, the microphone and the files to the machine are its. */
    let controlling = false;
    /** The rights of an administrator on the machine, as it told. */
    let rights = Admin.ASK;

    const screenWords: screen.Words = {
        display: (d, inUse) => `${d.name || fill(t.display, { n: d.screen + 1 })} · ${d.width}×${d.height}${inUse ? " · " + t.inUse : ""}`,
        stats: m => fill(t.stats, { w: m.w, h: m.h, fps: m.fps.toFixed(0), mbps: m.mbps.toFixed(1), capture: m.capture, encode: m.encode })
            + (m.latency < 0 ? "" : fill(t.latency, { ms: m.latency })),
    };
    const audioWords: audio.Words = {
        sound: on => on ? t.soundOn : t.soundOff,
        mic: on => on ? t.micOn : t.micOff,
        noSound: error => fill(t.noSound, { error }),
        micNotAllowed: t.micNotAllowed,
        micError: error => fill(t.micError, { error }),
        micFailed: t.micFailed,
    };
    const whys: Record<FileWhy, string> = {
        [FileWhy.CANCELLED]: t.whyCancelled,
        [FileWhy.PROTOCOL]: t.whyProtocol,
        [FileWhy.PERMISSION]: t.whyPermission,
        [FileWhy.DISK]: t.whyDisk,
        [FileWhy.IO]: t.whyIo,
        [FileWhy.OTHER]: t.whyOther,
        [FileWhy.CLOSED]: t.whyClosed,
        [FileWhy.ENDED]: t.whyEnded,
    };

    /** The line of a transfer of a file, in the words of the page. */
    const fileLine = (m: Extract<ToMain, { t: M.FILE_STATUS }>): string => {
        const name = m.name || t.aFile;
        let text: string;
        switch (m.line) {
            case FileLine.PROGRESS: text = fill(t.fileProgress, { name, n: m.n }); break;
            case FileLine.SAVED: text = fill(t.fileSaved, { name }); break;
            case FileLine.RECEIVED: text = fill(t.fileReceived, { name, n: m.n }); break;
            case FileLine.FAILED: text = fill(t.fileFailed, { name, why: whys[m.why] }) + (m.code < 0 ? "" : fill(t.fileCode, { code: m.code })); break;
            case FileLine.WAIT: text = t.fileWait; break;
            case FileLine.PICKING: text = t.filePicking; break;
            case FileLine.CANCELLED: text = t.fileCancelled; break;
        }
        return m.more > 0 ? text + fill(t.fileMore, { n: m.more }) : text;
    };

    const refusal = (m: Extract<ToMain, { t: M.DENIED }>): string => {
        const why = m.reason === Refusal.UNEXPECTED ? t.refusedUnexpected : m.reason === Refusal.WRONG_TOKEN ? t.refusedToken
            : m.reason === Refusal.OVERLOAD ? t.refusedOverload : m.reason === Refusal.FULL ? t.refusedFull : fill(t.refusedOther, { code: m.code });
        return m.left < 0 ? why : why + fill(t.attemptsLeft, { n: m.left });
    };

    function stopSession(): void {
        hearing?.stop();
        hearing = undefined;
        moving?.stop();
        moving = undefined;
        copying?.stop();
        copying = undefined;
        typing?.stop();
        typing = undefined;
        shown?.stop();
        shown = undefined;
        controlling = false;
        audienceLine.textContent = "";
        takeButton.hidden = true;
        if (document.fullscreenElement === root) void document.exitFullscreen();
    }

    const setLive = (on: boolean, host = "") => {
        live = on;
        root.classList.toggle("rd-on", on);
        options.onLive?.(on, host);
    };

    /** The desk closes: nothing more is dialed, the connection is let go. */
    const end = (text: string, isBad: boolean, wrongToken: boolean) => {
        if (ended) return;
        ended = true;
        stopRetrying();
        stopSession();
        post({ t: W.LEAVE });
        setTimeout(() => { worker.terminate(); URL.revokeObjectURL(workerUrl); }, 1000); // its connection closes first
        nextCanvas = undefined;
        root.remove();
        options.onEnded?.(text, isBad, wrongToken);
    };

    // ── reconnection: a session that broke is dialed again, a few times, further apart each time ──

    let retry = 0;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;

    function stopRetrying(): void {
        retry = 0;
        if (retryTimer !== undefined) clearTimeout(retryTimer);
        retryTimer = undefined;
    }

    const retryLater = () => {
        if (options.channel !== undefined) { stage("lostDirect"); return end(t.directLost, true, false); }
        if (retry >= RETRIES) { stage("noAnswer"); return end(t.noAnswer, true, false); }
        retry++;
        const seconds = Math.min(30, 2 ** (retry - 1));
        stage("lost");
        status(fill(t.lost, { seconds, attempt: retry, of: RETRIES }), true);
        retryTimer = setTimeout(() => { retryTimer = undefined; connect(); }, seconds * 1000);
    };

    function connect(): void {
        if (ended) return;
        const channel = options.channel;
        if (channel === undefined && !/^wss?:\/\//.test(address)) return end(t.badAddress, true, false);
        if (token === "") return end(t.noToken, true, false);
        showKey("", false);
        stage("dialing");
        status(t.connecting, false);
        const kept = channel === undefined ? store.get("host." + address) : undefined;
        const next = screen.canvasFor();
        nextCanvas = next.canvas;
        // direct: the data channel stays here, the worker gets its bytes over a port
        let port: MessagePort | undefined;
        if (channel !== undefined) {
            const pair = new MessageChannel();
            relay(channel, pair.port1);
            port = pair.port2;
        }
        post({
            t: W.CONNECT, address, token, name: options.name.trim() || navigator.platform || "a browser",
            secret: secret(), kept: kept === undefined ? undefined : unhex(kept), origin: performance.timeOrigin, canvas: next.offscreen, port,
        }, port === undefined ? [next.offscreen] : [next.offscreen, port]);
    }

    function welcome(host: string | undefined, administrator: boolean): void {
        who.textContent = (host ?? t.machine) + " · " + address;
        stopSession();
        stage("live");
        setLive(true, host ?? ""); // seen before the screen measures itself
        const canvas = nextCanvas ?? document.createElement("canvas"); // the worker draws on the one it was given
        nextCanvas = undefined;
        shown = screen.start(canvas, screenHost, root, displaySelect, stats, screenWords, () => seen, post);
        moving = files.start(transfer, cancelButton);
        notice.textContent = "";
        hearing = audio.start(soundButton, micButton, text => { notice.textContent = text; }, audioWords, post);
        if (framerate !== DEFAULT_FPS) post({ t: W.FPS, fps: framerate });
        stopRetrying();
        admin(administrator ? Admin.YES : Admin.ASK);
        control(false); // until the machine tells who controls: `Audience` follows the welcome
    }

    // ── the control: every viewer sees, one controls ──

    /** This viewer controls the machine, or only watches: the input and the clipboard are taken, or let go; the buttons of what only
     *  the one that controls does are on, or off. */
    function control(on: boolean): void {
        controlling = on;
        takeButton.hidden = on;
        for (const b of [sendButton, fitButton, lockButton]) b.disabled = !on;
        admin(rights);
        hearing?.control(on);
        const v = shown;
        if (on && v && !typing) {
            const before = document.activeElement as HTMLElement | null;
            typing = input.start(v.canvas, root, () => v.frameWidth(), post);
            // the keys stay where they were - another desk, a field of the page - unless they were in this desk, or nowhere
            if (!seen) root.querySelector("textarea")?.blur(); // a hidden desk does not take the keys
            else if (before && before !== document.body && !root.contains(before)) before.focus({ preventScroll: true });
            copying = clipboard.start(v.canvas, () => root.contains(document.activeElement), post);
            if (fitting) v.fit(true);
        } else if (!on && typing) {
            typing.stop();
            typing = undefined;
            copying?.stop();
            copying = undefined;
            if (fitting) v?.fit(false); // the worker drops it: the machine keeps the fit until the one that made it leaves
        }
    }

    const audience = (watchers: number, yours: boolean, controller: string) => {
        audienceLine.textContent = fill(t.watching, { n: watchers }) + " · " + (yours ? t.youControl : fill(t.controls, { name: controller || t.aViewer }));
        control(yours);
    };

    takeButton.addEventListener("click", () => { if (shown) post({ t: W.TAKE_CONTROL }); });

    // ── the rights of an administrator on the machine: asked by the operator, confirmed by the user of the machine ──

    function admin(told: Admin): void {
        rights = told;
        if (told === Admin.NOT_NEEDED) {
            adminButton.textContent = t.adminNotNeeded;
            adminButton.title = t.adminNotNeededTitle;
            adminButton.disabled = true;
            return;
        }
        const yes = told === Admin.YES;
        adminButton.textContent = yes ? t.adminYes : t.adminAsk;
        adminButton.disabled = yes || !controlling;
        adminButton.title = yes ? t.adminYesTitle : t.adminAskTitle;
    }

    const elevations: Record<Elevation, string> = {
        [Elevation.NONE]: "",
        [Elevation.ASKING]: t.elevationAsking,
        [Elevation.CONFIRMED]: t.elevationConfirmed,
        [Elevation.DECLINED]: t.elevationDeclined,
        [Elevation.FAILED]: t.elevationFailed,
    };

    // ── the buttons ──

    adminButton.addEventListener("click", () => post({ t: W.ELEVATE }));
    lockButton.addEventListener("click", () => typing?.lockPointer());

    const fitLabel = () => { fitButton.textContent = fitting ? t.fitOn : t.fitOff; };
    fitButton.addEventListener("click", () => {
        fitting = !fitting;
        fitLabel();
        shown?.fit(fitting);
    });
    fitLabel();

    let framerate = store.get("fps") === "60" ? 60 : DEFAULT_FPS;
    const fpsLabel = () => { fpsButton.textContent = fill(t.fps, { n: framerate }); };
    fpsButton.addEventListener("click", () => {
        framerate = framerate === 60 ? DEFAULT_FPS : 60;
        store.set("fps", framerate === DEFAULT_FPS ? undefined : String(framerate));
        fpsLabel();
        if (shown) post({ t: W.FPS, fps: framerate });
    });
    fpsLabel();

    sendButton.addEventListener("click", () => pick.click());
    pick.addEventListener("change", () => {
        if (moving && controlling && pick.files?.length) post({ t: W.FILES, files: [...pick.files] });
        pick.value = "";
    });
    getButton.addEventListener("click", () => { if (moving) post({ t: W.FILE_REQUEST }); });
    cancelButton.addEventListener("click", () => { if (moving) post({ t: W.FILE_CANCEL }); });

    // files dropped on the screen go to the machine - of the viewer that controls it
    screenHost.addEventListener("dragover", e => {
        if (!moving || !controlling || !e.dataTransfer?.types.includes("Files")) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = "copy";
        screenHost.classList.add("rd-drop");
    });
    screenHost.addEventListener("dragleave", () => screenHost.classList.remove("rd-drop"));
    screenHost.addEventListener("drop", e => {
        screenHost.classList.remove("rd-drop");
        if (!moving || !controlling || !e.dataTransfer?.files.length) return;
        e.preventDefault();
        post({ t: W.FILES, files: [...e.dataTransfer.files] });
    });

    fullButton.addEventListener("click", async () => {
        if (document.fullscreenElement === root) return void document.exitFullscreen();
        await root.requestFullscreen().catch(() => undefined);
        // in full screen the keys of the system - Alt+Tab, the Windows key - go to the machine too
        await (navigator as Navigator & { keyboard?: { lock?: () => Promise<void> } }).keyboard?.lock?.().catch(() => undefined);
    });

    const leave = () => { stage("left"); end(t.left, false, false); };
    leaveButton.addEventListener("click", leave);

    // ── what the worker tells ──

    const me = {
        failed(error: string) { status(fill(t.sessionFailed, { error }), true); },
        take(m: ToMain) {
            switch (m.t) {
                case M.PROVED: return proved(m.key, m.accepted);
                case M.STATUS: stage("failed"); return status(fill(t.linkFailed, { error: m.error }), true);
                case M.DIALED:
                    if (m.ok) { stage("reached"); return status(t.loggingIn, false); }
                    stage("unreachable");
                    if (m.error) status(fill(t.dialFailed, { error: m.error }), true);
                    if (!bad) status(fill(t.cannotConnect, { address }), true);
                    if (retry > 0) return retryLater();
                    return end(stateLine.textContent ?? "", true, false);
                case M.WELCOME: return welcome(m.host, m.administrator);
                case M.DENIED: stage("refused"); return end(refusal(m), true, m.forget);
                case M.AUDIENCE: return audience(m.watchers, m.yours, m.controller);
                case M.CLOSED:
                    if (live) {
                        stopSession();
                        setLive(false);
                    } else if (retry === 0) { stage("closed"); return end(t.closed, true, false); } // closed while logging in
                    status(t.closed, true);
                    return retryLater();
                case M.NOTICE: notice.textContent = elevations[m.elevation]; return;
                case M.ADMIN: return admin(m.admin);
                case M.SIZE:
                case M.SCREEN:
                case M.DISPLAYS:
                case M.CURSOR:
                case M.HOST_AT:
                case M.STATS:
                    return shown?.take(m);
                case M.CLIPBOARD: return copying?.set(m.text);
                case M.FILE_STATUS: return moving?.show(fileLine(m), m.busy);
                case M.DOWNLOAD: return moving?.save(m.blob, m.name);
                case M.MIC_ERROR: return hearing?.micFailed();
            }
        },
    };
    worker.onmessage = (e: MessageEvent<ToMain>) => me.take(e.data);
    worker.addEventListener("error", e => {
        console.error("the worker of the session failed:", e.message);
        me.failed(e.message);
    });
    connect();
    return {
        root,
        leave,
        show(on) {
            if (ended || on === seen) return;
            seen = on;
            root.style.display = on ? "" : "none";
            shown?.visibility();
            if (on) {
                typing?.focus();
                copying?.sync();
            } else if (document.fullscreenElement === root) void document.exitFullscreen();
        },
    };
}
