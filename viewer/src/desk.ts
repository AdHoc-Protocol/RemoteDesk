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
import * as marks from "./marks";

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
    badAddress: "That is not a line of the remote computer: it is address:port/token, as the program printed it.",
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
    notTheLine: "Not the computer of the line: its key does not fit the line. Someone else may answer at that address: the token was not sent.",
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
    viewWhole: "Whole",
    viewOne: "1:1",
    viewZoom: "{percent}%",
    viewTitle: "Whole: the screen of the machine fits this window. 1:1: a pixel of it is a pixel here, as large as there - the pointer or a finger at an edge moves the view. Two fingers apart or together zoom it",
    fps: "{n} fps",
    fpsTitle: "The most frames a second the machine sends: 60 is smoother, and takes more of the link and of the machine",
    draw: "✏️ Draw",
    drawOn: "✏️ Drawing",
    drawTitle: "Draw over the screen of the machine for its user: a line, a tap for a ring - here -, an arrow with ➚. Each fades by itself; pressed again, the mouse and the fingers control the machine again",
    arrow: "➚ Arrow",
    arrowTitle: "A stroke is an arrow, from where it began to where it ends",
    colorTitle: "The colour of the marks",
    back: "◁",
    backTitle: "Back: the button of the phone",
    home: "○",
    homeTitle: "Home: the button of the phone",
    recents: "□",
    recentsTitle: "Recent apps: the button of the phone",
    handleTitle: "The bar: pull it down or press it, push it up to hide it",
    moreTitle: "More: the size, the frames, the pointer, the display",
    lock: "Lock pointer",
    lockTitle: "Mouse moves go to the machine as relative moves; Esc frees the pointer",
    full: "Full screen",
    fullTitle: "Full screen: the keys of the system go to the machine too",
    leave: "Disconnect",
    watching: "{n} watching",
    controls: "controls: {name}",
    youControl: "you control",
    aViewer: "a viewer",
    noInput: "✋ The phone does not let you control it yet",
    noInputTitle: "Its user turns it on once in the app RemoteDesk: Let the viewer control this phone. Until then you watch, and you can draw over its screen",
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

/** The words of an error of the network, as the user knows the address: the `ws://` of the page, and its closing slash, out. */
const bare = (error: string): string => error.replace(/\bws:\/\/([^\s/]+)\/?/g, "$1");

export interface Options {
    /** `ws://…` or `wss://…`: where the machine listens; with `channel` - only what the bar names. */
    address: string;
    /** Way 4, direct: the data channel the page and the machine met on ( ./direct.ts ) - the session goes over it, given to the
     *  worker. Lost, it is not dialed again: a new one takes a new exchange of the lines. */
    channel?: RTCDataChannel | undefined;
    /** What the line of the machine carries of its key, after its token: the key the machine proves must fit it. */
    check?: string | undefined;
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
 * live session broke, dialed again; `lostDirect` - the direct one broke; `noAnswer` - dialed again, in vain; `left` - the user left;
 * `notTheLine` - the key the machine proved does not fit the check in its line: not the machine of the line, the token not sent.
 */
export type Stage = "dialing" | "reached" | "proved" | "otherKey" | "live" | "unreachable" | "refused" | "closed" | "failed" | "lost"
    | "lostDirect" | "noAnswer" | "left" | "notTheLine";

export interface Desk {
    /** The element of the desk, in the container. */
    readonly root: HTMLElement;
    /** Leaves the machine: the desk closes. */
    leave(): void;
    /** Shows the desk, or hides it: a hidden one takes no video; a shown one takes the keys. */
    show(on: boolean): void;
    /** `root` went into another window ( or back ): the screen, the keys and the clipboard follow it there. */
    moved(): void;
}

/**
 * The line a machine prints for its viewer - `<address>:<port>/<token>` -: its address and token; `undefined` - not such a line.
 * A program of before printed # in place of /: WeChat takes # for a tag and copies the line cut, so it is / now, and # is taken too.
 * The line is found in what was pasted as it came: a messenger puts marks no one sees around an address, non-breaking spaces, words
 * of the message around it; a keyboard of Chinese or Japanese types ＃ and ： of full width; a copy from the console takes its
 * frame. NFKC makes the wide ones plain; the rest is left aside.
 */
export function splitLine(line: string): { address: string; token: string } | undefined {
    const text = line.normalize("NFKC").replace(/\p{Cf}/gu, "").replace(/ /g, " ");
    const m = /((?:wss?:\/\/)?(?:\[[0-9a-f:.]+\]|[a-z0-9.-]+)(?::\d{1,5})?)[/#](\S+)/i.exec(text);
    return m === null ? undefined : { address: m[1]!, token: m[2]! };
}

/** Forgets the key kept for `address`: the next login keeps the one the machine shows then. */
export function forgetKey(store: Store, address: string): void {
    store.set("host." + address.trim(), undefined);
}

const hex = (bytes: Uint8Array) => Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");
const unhex = (text: string) => Uint8Array.from({ length: text.length / 2 }, (_, i) => parseInt(text.slice(i * 2, i * 2 + 2), 16));
const fingerprint = (key: Uint8Array) => hex(key).replace(/(.{4})(?!$)/g, "$1 ");

// ── the icons of the bar: strokes of 24 × 24, the colour of the text ──

const ICONS: Record<string, string> = {
    back: '<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>',
    home: '<circle cx="12" cy="12" r="8"/>',
    recents: '<rect x="5" y="5" width="14" height="14" rx="2"/>',
    draw: '<path d="M21.17 6.81a2.83 2.83 0 0 0-4-4L3.84 16.17a2 2 0 0 0-.5.83l-1.32 4.35a.5.5 0 0 0 .62.62l4.35-1.32a2 2 0 0 0 .83-.5z"/>',
    arrow: '<path d="M7 7h10v10"/><path d="M7 17 17 7"/>',
    sound: '<path d="M11 4.7a.7.7 0 0 0-1.2-.5L6.4 7.6A1.4 1.4 0 0 1 5.4 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.4a1.4 1.4 0 0 1 1 .4l3.4 3.4a.7.7 0 0 0 1.2-.5z"/><path class="rd-on-only" d="M16 9a5 5 0 0 1 0 6"/><path class="rd-on-only" d="M19.4 18.4a9 9 0 0 0 0-12.8"/><path class="rd-off-only" d="m22 9-6 6"/><path class="rd-off-only" d="m16 9 6 6"/>',
    mic: '<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v3"/><path class="rd-off-only" d="M2 2l20 20"/>',
    send: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m17 8-5-5-5 5"/><path d="M12 3v12"/>',
    get: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>',
    full: '<path d="M8 3H5a2 2 0 0 0-2 2v3"/><path d="M21 8V5a2 2 0 0 0-2-2h-3"/><path d="M3 16v3a2 2 0 0 0 2 2h3"/><path d="M16 21h3a2 2 0 0 0 2-2v-3"/>',
    more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
    leave: '<path d="m19 5 3-3"/><path d="m2 22 3-3"/><path d="M6.3 20.3a2.4 2.4 0 0 0 3.4 0L12 18l-6-6-2.3 2.3a2.4 2.4 0 0 0 0 3.4Z"/><path d="M7.5 13.5 10 11"/><path d="M10.5 16.5 13 14"/><path d="m12 6 6 6 2.3-2.3a2.4 2.4 0 0 0 0-3.4l-2.6-2.6a2.4 2.4 0 0 0-3.4 0Z"/>',
};

/** The SVG of an icon of `ICONS`. */
const svg = (name: string) =>
    `<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name] ?? ""}</svg>`;

// ── the style, once a page ──

const STYLE = `
.rd-desk{--rd-bg:#f6f7f9;--rd-fg:#1d2330;--rd-muted:#5b6475;--rd-line:#d5d9e0;--rd-accent:#2f6fde;--rd-bad:#c4352b;--rd-good:#2a9d4b;--rd-wait:#c98a00;--rd-panel:#ffffff;--rd-hover:#e9ecf1;position:relative;display:flex;flex-direction:column;height:100%;min-height:0;box-sizing:border-box;background:var(--rd-bg);color:var(--rd-fg);font:14px/1.45 system-ui,sans-serif}
@media (prefers-color-scheme:dark){.rd-desk{--rd-bg:#15181e;--rd-fg:#e4e7ec;--rd-muted:#98a0ae;--rd-line:#2c323c;--rd-accent:#6b9bf2;--rd-bad:#ef6a5f;--rd-good:#3fb950;--rd-wait:#d8a32a;--rd-panel:#111317;--rd-hover:#262b33}}
.rd-desk *{box-sizing:border-box}
.rd-desk{container-type:inline-size}
.rd-narrow{display:none!important}
@container (max-width:560px){.rd-who,.rd-audience,.rd-sep,.rd-wide{display:none!important}.rd-narrow:not([hidden]){display:block!important}}
.rd-drawer{position:absolute;left:0;right:0;top:0;z-index:5;transition:transform .18s ease}
.rd-drawer.rd-dragging{transition:none}
.rd-bar{display:flex;align-items:center;gap:2px;height:40px;padding:0 8px;background:var(--rd-panel);border-bottom:1px solid var(--rd-line);color:var(--rd-muted);white-space:nowrap;overflow-x:auto;scrollbar-width:none}
.rd-bar::-webkit-scrollbar{display:none}
.rd-bar button,.rd-menu button{margin:0;padding:0 8px;height:30px;border:0;border-radius:8px;background:transparent;color:var(--rd-accent);font:inherit;cursor:pointer;white-space:nowrap;flex:none}
.rd-bar button:hover:not(:disabled),.rd-menu button:hover:not(:disabled){background:var(--rd-hover)}
.rd-bar button:disabled,.rd-menu button:disabled{color:var(--rd-muted);cursor:default}
.rd-ib{width:32px;padding:0!important;display:inline-flex;align-items:center;justify-content:center;color:var(--rd-fg)!important}
.rd-ib[aria-pressed="true"]{background:var(--rd-accent)!important;color:#fff!important}
.rd-ib.rd-leave{color:var(--rd-bad)!important}
.rd-ib[data-on="false"] .rd-on-only,.rd-ib[data-on="true"] .rd-off-only,.rd-ib:not([data-on]) .rd-off-only{display:none}
.rd-ib[data-on="false"]{color:var(--rd-muted)!important}
.rd-sep{flex:none;width:1px;height:18px;margin:0 6px;background:var(--rd-line)}
.rd-dot{flex:none;width:9px;height:9px;margin:0 8px 0 4px;border-radius:50%;background:var(--rd-wait)}
.rd-desk.rd-on .rd-dot{background:var(--rd-good)}
.rd-desk.rd-on.rd-watching .rd-dot{background:var(--rd-accent)}
.rd-who{color:var(--rd-fg);font-weight:600;overflow:hidden;text-overflow:ellipsis;max-width:28ch}
.rd-audience{margin-left:8px}
.rd-swatch{width:14px;height:14px;border-radius:50%;background:currentColor}
.rd-view{font:12px ui-monospace,monospace!important;color:var(--rd-fg)!important}
.rd-handle{display:flex;align-items:center;justify-content:center;width:64px;height:16px;margin:0 auto;background:var(--rd-panel);border:1px solid var(--rd-line);border-top:0;border-radius:0 0 10px 10px;cursor:grab;touch-action:none}
.rd-handle:active{cursor:grabbing}
.rd-grip{width:26px;height:7px;background:repeating-linear-gradient(to bottom,var(--rd-muted) 0 1.5px,transparent 1.5px 3px)}
.rd-desk:not(.rd-on) .rd-handle{display:none}
.rd-menu{position:absolute;right:8px;top:44px;z-index:6;display:flex;flex-direction:column;align-items:stretch;min-width:220px;max-width:min(320px,calc(100% - 16px));padding:6px;border:1px solid var(--rd-line);border-radius:10px;background:var(--rd-panel);color:var(--rd-muted)}
.rd-menu[hidden]{display:none}
.rd-menu button{text-align:left}
.rd-menu select{margin:4px 8px;padding:2px 6px;border:1px solid var(--rd-line);border-radius:6px;background:transparent;color:var(--rd-fg);font:inherit}
.rd-menu .rd-stats{padding:4px 8px 6px;border-bottom:1px solid var(--rd-line);margin-bottom:4px;white-space:normal}
.rd-grow{flex:1}
.rd-stats{min-width:0;font:12px ui-monospace,monospace;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.rd-notice{color:var(--rd-bad)}
.rd-transfer{max-width:40ch;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;margin:0 8px}
.rd-audience{max-width:40ch;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.rd-noinput{color:var(--rd-bad);font-weight:600;white-space:nowrap}
.rd-screen{flex:1;min-height:0;display:flex;align-items:center;justify-content:center;background:#000;position:relative}
.rd-draw{position:absolute;inset:0;display:none;z-index:3;cursor:crosshair;touch-action:none}
.rd-draw.rd-drawing{display:block}
.rd-screen.rd-drop{outline:3px dashed var(--rd-accent);outline-offset:-3px}
.rd-screen canvas{max-width:100%;max-height:100%;outline:none}
.rd-screen.rd-one{display:block;position:relative;overflow:hidden}
.rd-screen.rd-one canvas{max-width:none;max-height:none;position:absolute;left:0;top:0}
.rd-state{flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:8px;padding:64px 24px 24px;text-align:center}
.rd-state .rd-bad{color:var(--rd-bad)}
.rd-key{max-width:60ch;color:var(--rd-muted);font:12px/1.4 ui-monospace,monospace;word-break:break-all}
.rd-desk:not(.rd-on) .rd-s,.rd-desk:not(.rd-on) .rd-screen,.rd-desk.rd-on .rd-state{display:none}
.rd-bar [hidden],.rd-menu [hidden]{display:none!important}
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
    /** A button of the bar showing an icon of `ICONS`: what it does in its title. */
    const iconButton = (className: string, icon: string, title: string) => {
        const b = button("rd-s rd-ib " + className, "", title);
        b.innerHTML = svg(icon);
        b.setAttribute("aria-label", title);
        b.dataset["help"] = title;
        return b;
    };
    const separator = () => element("span", "rd-s rd-sep");
    const bar = element("div", "rd-bar");
    const dot = element("span", "rd-dot");
    const who = element("span", "rd-who", options.title || address);
    const audienceLine = element("span", "rd-s rd-audience");
    // the machine takes no input: a phone whose user has not let RemoteDesk control it
    const noInputLine = element("span", "rd-s rd-noinput", t.noInput, t.noInputTitle);
    noInputLine.hidden = true;
    const takeButton = button("rd-s", t.takeControl, t.takeControlTitle);
    takeButton.hidden = true;
    const displaySelect = element("select", "rd-s", "", t.displayTitle);
    displaySelect.hidden = true;
    const stats = element("div", "rd-stats");
    const transfer = element("span", "rd-s rd-transfer");
    const cancelButton = button("rd-s", t.cancel, t.cancelTitle);
    cancelButton.hidden = true;
    const notice = element("span", "rd-s rd-notice");
    const soundButton = iconButton("", "sound", t.soundTitle);
    const micButton = iconButton("", "mic", t.micTitle);
    const sendButton = iconButton("rd-wide", "send", t.send + ": " + t.sendTitle);
    const getButton = iconButton("rd-wide", "get", t.get + ": " + t.getTitle);
    const pick = element("input", "");
    pick.type = "file";
    pick.multiple = true;
    pick.hidden = true;
    const adminButton = button("rd-s", t.adminAsk, t.adminAskTitle);
    const fitButton = button("rd-s", "", t.fitTitle);
    const viewButton = button("rd-s rd-view", "", t.viewTitle);
    const fpsButton = button("rd-s", "", t.fpsTitle);
    const lockButton = button("rd-s", t.lock, t.lockTitle);
    const drawButton = iconButton("", "draw", t.drawTitle);
    // the buttons of a phone: Back, Home, Recents - the keys of the consumer page it takes for them
    const backButton = iconButton("", "back", t.backTitle);
    const homeButton = iconButton("", "home", t.homeTitle);
    const recentsButton = iconButton("", "recents", t.recentsTitle);
    const phoneSeparator = separator();
    for (const b of [backButton, homeButton, recentsButton, phoneSeparator]) b.hidden = true;
    const arrowButton = iconButton("", "arrow", t.arrowTitle);
    arrowButton.hidden = true;
    const colorButton = button("rd-s rd-ib", "", t.colorTitle);
    colorButton.append(element("span", "rd-swatch"));
    colorButton.hidden = true;
    const fullButton = iconButton("rd-wide", "full", t.full + ": " + t.fullTitle);
    const moreButton = iconButton("", "more", t.moreTitle);
    const leaveButton = iconButton("rd-leave", "leave", t.leave);
    leaveButton.classList.remove("rd-s"); // shown before the machine lets the page in, too
    // ── the bar: who and how, the buttons by what they are for; what is rare in the menu of ⋯ ──
    bar.append(dot, who, audienceLine, noInputLine, takeButton, transfer, cancelButton, notice, element("span", "rd-grow"),
        backButton, homeButton, recentsButton, phoneSeparator,
        drawButton, arrowButton, colorButton, soundButton, micButton, separator(),
        sendButton, getButton, pick, separator(),
        viewButton, fullButton, moreButton, separator(), leaveButton);
    const menu = element("div", "rd-menu");
    menu.hidden = true;
    // on a narrow screen ( a phone ) the files and the full screen are in the menu: the bar has no room for them
    const inMenu = (of: HTMLButtonElement, text: string) => {
        const b = button("rd-narrow", text, of.dataset["help"] ?? "");
        b.addEventListener("click", () => of.click());
        return b;
    };
    menu.append(stats, displaySelect, inMenu(sendButton, t.send), inMenu(getButton, t.get), inMenu(fullButton, t.full), fitButton, fpsButton, lockButton, adminButton);
    // the drawer: the bar over the top of the screen, pulled down by its handle and pushed up again
    const drawer = element("div", "rd-drawer");
    const handle = element("div", "rd-handle", "", t.handleTitle);
    handle.append(element("span", "rd-grip"));
    drawer.append(bar, handle, menu);
    const screenHost = element("div", "rd-screen");
    const state = element("div", "rd-state");
    const stateLine = element("div", "");
    const keyLine = element("div", "rd-key");
    state.append(stateLine, keyLine);
    root.append(screenHost, state, drawer);
    container.appendChild(root);

    // ── how it goes ──

    let bad = false;
    const status = (text: string, isBad: boolean) => {
        if (ended) return; // why it ended stays: what the closing connection tells after it does not cover it
        bad = isBad;
        stateLine.textContent = text;
        stateLine.className = isBad ? "rd-bad" : "";
        options.onStatus?.(text, isBad);
    };

    const stage = (s: Stage) => { if (!ended) options.onStage?.(s); };

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
    const proved = (key: Uint8Array, accepted: boolean, checked: boolean) => {
        // direct: the machine is the one whose line came by hand - no address to keep its key for
        stage(accepted ? "proved" : "otherKey");
        if (options.channel !== undefined) return showKey(fill(t.keyIs, { key: fingerprint(key) }), false);
        // checked by the line: it is the machine - its key is kept, a new one too; not it - nothing more goes there
        if (checked) {
            if (!accepted) {
                stage("notTheLine");
                return end(t.notTheLine, true, false);
            }
            store.set("host." + address, hex(key));
            return showKey(fill(t.keyIs, { key: fingerprint(key) }), false);
        }
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
    let marking: marks.Marking | undefined;
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
        marking?.stop();
        marking = undefined;
        shown?.stop();
        shown = undefined;
        controlling = false;
        audienceLine.textContent = "";
        takeButton.hidden = true;
        if (root.ownerDocument.fullscreenElement === root) void root.ownerDocument.exitFullscreen();
    }

    const setLive = (on: boolean, host = "") => {
        live = on;
        root.classList.toggle("rd-on", on);
        placeDrawer();
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
            t: W.CONNECT, address, token, check: channel === undefined ? options.check : undefined, name: options.name.trim() || navigator.platform || "a browser",
            secret: secret(), kept: kept === undefined ? undefined : unhex(kept), origin: performance.timeOrigin, canvas: next.offscreen, port,
        }, port === undefined ? [next.offscreen] : [next.offscreen, port]);
    }

    /** The machine is a phone: its buttons, and the characters typed go as text. */
    let phone = false;

    function welcome(host: string | undefined, administrator: boolean, isPhone: boolean): void {
        phone = isPhone;
        for (const b of [backButton, homeButton, recentsButton, phoneSeparator]) b.hidden = !phone;
        adminButton.hidden = phone; // a phone has no administrator to become
        who.textContent = (host ?? t.machine) + " · " + address.replace(/^ws:\/\//, ""); // the scheme is the page's, not the user's
        stopSession();
        stage("live");
        setLive(true, host ?? ""); // seen before the screen measures itself
        const canvas = nextCanvas ?? document.createElement("canvas"); // the worker draws on the one it was given
        nextCanvas = undefined;
        shown = screen.start(canvas, screenHost, root, displaySelect, stats, screenWords, () => seen, post, viewLabel);
        if (oneToOne) shown.oneToOne(true);
        marking = marks.start(screenHost, () => shown?.canvas, post, () => markColor, () => markArrow);
        marking.on(drawing);
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
        root.classList.toggle("rd-watching", !on);
        takeButton.hidden = on;
        for (const b of [sendButton, fitButton, lockButton]) b.disabled = !on;
        admin(rights);
        hearing?.control(on);
        const v = shown;
        if (on && v && !typing) {
            const before = root.ownerDocument.activeElement as HTMLElement | null;
            typing = input.start(v.canvas, root, () => v.frameWidth(), post, (phase, ratio, x, y) => v.pinch(phase, ratio, x, y), phone);
            // the keys stay where they were - another desk, a field of the page - unless they were in this desk, or nowhere
            if (!seen) root.querySelector("textarea")?.blur(); // a hidden desk does not take the keys
            else if (before && before !== root.ownerDocument.body && !root.contains(before)) before.focus({ preventScroll: true });
            copying = clipboard.start(v.canvas, () => root.contains(root.ownerDocument.activeElement), post);
            if (fitting) v.fit(true);
        } else if (!on && typing) {
            typing.stop();
            typing = undefined;
            copying?.stop();
            copying = undefined;
            if (fitting) v?.fit(false); // the worker drops it: the machine keeps the fit until the one that made it leaves
        }
    }

    const audience = (watchers: number, yours: boolean, controller: string, noInput: boolean) => {
        audienceLine.textContent = fill(t.watching, { n: watchers }) + " · " + (yours ? t.youControl : fill(t.controls, { name: controller || t.aViewer }));
        noInputLine.hidden = !noInput;
        dot.title = audienceLine.textContent ?? "";
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
    const press = (usb: number) => () => {
        if (!controlling) return;
        post({ t: W.KEY, usb, pressed: true });
        post({ t: W.KEY, usb, pressed: false });
        typing?.focus();
    };
    backButton.addEventListener("click", press(0x0c0224));
    homeButton.addEventListener("click", press(0x0c0223));
    recentsButton.addEventListener("click", press(0x0c01a2));

    // ── the drawer of the bar: down by its handle, a press, or a pull; up the same way - the screen of the machine under it ──

    let barOpen = store.get("bar.open") !== "0";
    /** The drawer where it is to be, or `offset` pixels down from closed while it is pulled. */
    const placeDrawer = (offset?: number) => {
        const height = bar.offsetHeight || 40;
        if (offset !== undefined) drawer.style.transform = `translateY(${offset}px)`;
        else drawer.style.transform = barOpen || !live ? "" : `translateY(${-height}px)`;
        if (!barOpen && live) closeMenu();
    };
    let pull: { y: number; from: number; moved: boolean } | undefined;
    const pulled = (e: PointerEvent) => Math.min(0, Math.max(-(bar.offsetHeight || 40), pull!.from + e.clientY - pull!.y));
    handle.addEventListener("pointerdown", e => {
        handle.setPointerCapture(e.pointerId);
        pull = { y: e.clientY, from: barOpen ? 0 : -(bar.offsetHeight || 40), moved: false };
        drawer.classList.add("rd-dragging");
    });
    handle.addEventListener("pointermove", e => {
        if (!pull) return;
        if (Math.abs(e.clientY - pull.y) > 4) pull.moved = true;
        if (pull.moved) placeDrawer(pulled(e));
    });
    const letGo = (e: PointerEvent) => {
        if (!pull) return;
        barOpen = pull.moved ? pulled(e) > -(bar.offsetHeight || 40) / 2 : !barOpen; // a press: the other way
        pull = undefined;
        drawer.classList.remove("rd-dragging");
        store.set("bar.open", barOpen ? "1" : "0");
        placeDrawer();
    };
    handle.addEventListener("pointerup", letGo);
    handle.addEventListener("pointercancel", letGo);

    // the menu of ⋯: what is set once in a while
    const closeMenu = () => {
        menu.hidden = true;
        moreButton.setAttribute("aria-pressed", "false");
    };
    moreButton.addEventListener("click", () => {
        menu.hidden = !menu.hidden;
        moreButton.setAttribute("aria-pressed", String(!menu.hidden));
    });
    root.addEventListener("pointerdown", e => {
        const at = e.target as Node;
        if (!menu.hidden && !menu.contains(at) && !moreButton.contains(at)) closeMenu();
    }, true);

    // ── marks over the screen of the machine: any viewer draws ──

    const MARK_COLORS = ["#f03e3e", "#ffc41a", "#2ec25a", "#3b82f6"];
    let drawing = false, markArrow = false;
    let markColor = Math.min(3, Math.max(0, Number(store.get("mark.color") ?? 0) | 0));
    const markLabels = () => {
        drawButton.title = drawing ? t.drawOn : t.drawTitle;
        drawButton.setAttribute("aria-pressed", String(drawing));
        arrowButton.hidden = colorButton.hidden = !drawing;
        arrowButton.setAttribute("aria-pressed", String(markArrow));
        colorButton.style.color = MARK_COLORS[markColor];
    };
    markLabels();
    drawButton.addEventListener("click", () => {
        drawing = !drawing;
        marking?.on(drawing);
        markLabels();
    });
    arrowButton.addEventListener("click", () => {
        markArrow = !markArrow;
        markLabels();
    });
    colorButton.addEventListener("click", () => {
        markColor = (markColor + 1) % MARK_COLORS.length;
        store.set("mark.color", String(markColor));
        markLabels();
    });

    /** 1:1 or the whole screen: kept on the device, for every machine; a zoom of a pinch is not kept. */
    let oneToOne = store.get("view") === "1:1";
    /** The view as the screen tells it: the button says it; pressed, a zoom or 1:1 goes back to the whole screen, the whole to 1:1. */
    let view: screen.View = { kind: oneToOne ? "one" : "whole" };
    function viewLabel(now: screen.View = view): void {
        view = now;
        viewButton.textContent = now.kind === "one" ? t.viewOne : now.kind === "zoom" ? fill(t.viewZoom, { percent: now.percent }) : t.viewWhole;
    }
    viewButton.addEventListener("click", () => {
        oneToOne = view.kind === "whole";
        store.set("view", oneToOne ? "1:1" : undefined);
        viewLabel({ kind: oneToOne ? "one" : "whole" });
        shown?.oneToOne(oneToOne);
    });
    viewLabel();

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
        if (root.ownerDocument.fullscreenElement === root) return void root.ownerDocument.exitFullscreen();
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
                case M.PROVED: return proved(m.key, m.accepted, m.checked);
                case M.STATUS: stage("failed"); return status(fill(t.linkFailed, { error: bare(m.error) }), true);
                case M.DIALED:
                    if (m.ok) { stage("reached"); return status(t.loggingIn, false); }
                    stage("unreachable");
                    if (m.error) status(fill(t.dialFailed, { error: bare(m.error) }), true);
                    if (!bad) status(fill(t.cannotConnect, { address }), true);
                    if (retry > 0) return retryLater();
                    return end(stateLine.textContent ?? "", true, false);
                case M.WELCOME: return welcome(m.host, m.administrator, m.phone);
                case M.DENIED: stage("refused"); return end(refusal(m), true, m.forget);
                case M.AUDIENCE: return audience(m.watchers, m.yours, m.controller, m.noInput);
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
                case M.PICTURE: return copying?.setPicture(m.png);
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
            } else if (root.ownerDocument.fullscreenElement === root) void root.ownerDocument.exitFullscreen();
        },
        moved() {
            shown?.moved();
            typing?.moved();
            copying?.moved();
            if (seen) typing?.focus();
        },
    };
}
