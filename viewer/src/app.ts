// The viewer page of RemoteDesk: the login form, and the desk of the session ( ./desk.ts - the bar, the screen, the session in its
// worker ) put in place of the form while the machine has the page in. What the page keeps is kept on this device, in localStorage: the
// name typed last, the way, what the desk keeps, and the computers of the user - the name given to each, and its line, read by the
// camera or connected with, its token in it: one press connects again, or sends the line on to another device.

import { english, forgetKey, open, splitLine as split, type Desk, type Stage, type Store } from "./desk";
import { card, offer, STUN_NAMES, type Card, type Heard, type Offer } from "./direct";
import { drawScene } from "./scene";
import { scan } from "./scan";
import { createDockview, themeDark, themeLight, type DockviewApi, type IDockviewPanel } from "dockview";
import dockviewStyle from "dockview/dist/styles/dockview.css";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/** The hand the diagram is written in: the font, as data, put in by build.mjs. */
declare const HAND_FONT: string;
const hand = document.createElement("style");
hand.textContent = `@font-face { font-family: "RD Hand"; src: url(${HAND_FONT}) format("woff2"); font-weight: 400 700; font-display: block; }`;
document.head.append(hand);

const form = $<HTMLFormElement>("login");
const address = $<HTMLInputElement>("address");
const yourName = $<HTMLInputElement>("name");
const statusLine = $<HTMLDivElement>("status");
const keyLine = $<HTMLDivElement>("key");
const forget = $<HTMLButtonElement>("forget");
const deskHost = $<HTMLDivElement>("desk");
const map = $<HTMLElement>("map");
const wayButtons = Array.from(document.querySelectorAll<HTMLButtonElement>("#ways button"));
const scene = document.getElementById("scene") as unknown as SVGSVGElement;
const midSay = $<HTMLParagraphElement>("mid-say"), remSay = $<HTMLParagraphElement>("rem-say");
/** The parts of the diagram, by the attribute of #map that tells the state of each: the style draws it. */
const nodeMe = "me", nodeMid = "mid", nodeRem = "rem", wireA = "wa", wireB = "wb", arrowOut = "out", arrowBack = "back";
type Part = typeof nodeMe | typeof nodeMid | typeof nodeRem | typeof wireA | typeof wireB | typeof arrowOut | typeof arrowBack;
const backNumber = $<HTMLSpanElement>("back-n"), backLabel = $<HTMLLabelElement>("back-label");
const directStart = $<HTMLButtonElement>("direct-start");
const offerSaid = $<HTMLParagraphElement>("offer-said");
const offerTold = $<HTMLSpanElement>("offer-told");
const offerLine = $<HTMLTextAreaElement>("offer-line");
const offerCopy = $<HTMLButtonElement>("offer-copy");
const log = $<HTMLDetailsElement>("log"), feed = $<HTMLOListElement>("feed");
const scanButton = $<HTMLButtonElement>("scan");
const linesBox = $<HTMLElement>("lines"), linesList = $<HTMLOListElement>("lines-list");

// ── what the page keeps on this device ──

const store: Store = {
    get(name: string): string | undefined { try { return localStorage.getItem("rdp." + name) ?? undefined; } catch { return undefined; } },
    set(name: string, value: string | undefined): void {
        try { value === undefined ? localStorage.removeItem("rdp." + name) : localStorage.setItem("rdp." + name, value); } catch { /* a private window */ }
    },
};

type Kind = "" | "bad" | "ok" | "wait";

// ── the machines connected at once: a panel of dockview each ──
//
// A machine let in is a panel: tabs in a group, and a tab dragged to an edge splits the screen - a mosaic -, out of the groups it
// floats over them ( ▣ ). ⧉ takes a machine into a window of its own, anywhere on the screens; closed, it comes back as a panel. Every desk
// seen takes the video; one behind the tab of another, or under the form, takes none - its machine neither captures nor encodes for
// it ( src/desk.ts ). + brings the form: another machine is connected beside the ones there; a desk that ends takes its panel with it.

const backButton = $<HTMLButtonElement>("to-desks");

/** A machine connected, or being connected: its desk; its panel once it let the page in, or its window of its own. */
interface Open {
    desk: Desk;
    /** Its name as it tells it, and its address. */
    title: string;
    panel: IDockviewPanel | undefined;
    window: Window | undefined;
    /** It went: its panel goes, the desk is not left again for it. */
    ended: boolean;
}
const opens: Open[] = [];
/** The form is shown: no desk is seen. */
let formShown = true;

const dockStyle = document.createElement("style");
dockStyle.textContent = dockviewStyle;
document.head.append(dockStyle);
const darkScheme = matchMedia("(prefers-color-scheme: dark)");

/** A small button of a group or a window. */
function action(text: string, title: string, act: () => void): HTMLButtonElement {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "rd-act";
    b.textContent = text;
    b.title = title;
    b.addEventListener("click", e => { e.stopPropagation(); act(); });
    return b;
}

const dock: DockviewApi = createDockview(deskHost, {
    theme: darkScheme.matches ? themeDark : themeLight,
    // the content of a panel: the element of the desk of its machine
    createComponent: () => {
        const element = document.createElement("div");
        element.className = "rd-panel";
        return { element, init: p => { element.append((p.params as { open: Open }).open.desk.root); } };
    },
    // + and ⧉ at the right of each group
    createRightHeaderActionComponent: group => {
        const element = document.createElement("div");
        element.className = "rd-acts";
        element.append(
            action("+", "Connect to another computer as well: the ones connected stay", () => showForm(true)),
            action("▣", "Over the others, anywhere on this page; once more: back among them", () => {
                if (group.api.location.type === "floating") group.api.moveTo({ position: "right" });
                else dock.addFloatingGroup(group, { width: 640, height: 420 });
            }),
            action("⧉", "Into a window of its own: put it anywhere on your screens; closed, it comes back here", () => {
                const o = opens.find(x => x.panel === group.activePanel);
                if (o) apart(o);
            }),
        );
        return { element, init: () => undefined, dispose: () => element.remove() };
    },
});
darkScheme.addEventListener("change", () => dock.updateOptions({ theme: darkScheme.matches ? themeDark : themeLight }));
dock.onDidRemovePanel(panel => {
    const o = opens.find(x => x.panel === panel);
    if (!o || o.window) return; // gone to a window of its own: the desk stays
    o.panel = undefined;
    if (!o.ended) o.desk.leave(); // ✕ of its tab
    seenAll();
});
dock.onDidLayoutChange(() => seenAll());

/** Each desk seen takes the video, the others not: what the form covers, or the tab of another, is not seen. */
function seenAll(): void {
    for (const o of opens) {
        if (o.window) o.desk.show(!o.window.closed);
        else if (o.panel) o.desk.show(!formShown && o.panel.api.isVisible);
    }
    const any = opens.some(o => o.panel || o.window);
    backButton.hidden = !any;
}

/** The form, or the panels. */
function showForm(on: boolean): void {
    formShown = on || !opens.some(o => o.panel);
    form.style.display = formShown ? "" : "none";
    deskHost.classList.toggle("on", !formShown);
    if (formShown && !busy) {
        where = "";
        address.value = "";
        draw();
    }
    seenAll();
    if (formShown) address.focus();
}

backButton.addEventListener("click", () => showForm(false));

/** `o` as a panel, in front. */
function panelOf(o: Open): void {
    o.panel = dock.addPanel({ id: `desk-${opens.indexOf(o)}-${Date.now()}`, component: "desk", title: o.title, params: { open: o } });
    o.panel.api.onDidVisibilityChange(() => seenAll());
    o.panel.api.setActive();
    o.desk.moved(); // from its own window, or from where it waited: the same window, nothing to do
}

/**
 * `o` in a window of its own: the panel goes, its desk with the style of the page into the window - the window shows what the page
 * draws there. Closed, or the page gone, it comes back as a panel.
 */
function apart(o: Open): void {
    const w = window.open("", `rd-${Date.now()}`, "width=1280,height=800");
    if (!w) return show("The browser did not let the page open a window: allow its pop-ups for this page.", "bad");
    w.document.title = o.title;
    for (const s of document.querySelectorAll("style")) w.document.head.append(s.cloneNode(true));
    w.document.body.style.cssText = "margin:0;height:100vh;background:#000";
    o.window = w;
    const panel = o.panel;
    o.panel = undefined;
    if (panel) dock.removePanel(panel);
    w.document.body.append(o.desk.root);
    o.desk.root.style.height = "100%";
    o.desk.moved();
    // back as a panel when the window closes
    const home = () => {
        if (o.window !== w) return;
        o.window = undefined;
        if (o.ended) return;
        panelOf(o);
        showForm(false);
    };
    w.addEventListener("pagehide", home);
    window.addEventListener("pagehide", () => w.close());
    seenAll();
}

/** A line of the log under the diagram: what happens, in detail, for whoever opens it. */
function note(text: string, kind: Kind = ""): void {
    const li = document.createElement("li"), t = document.createElement("span");
    t.className = "t";
    t.textContent = new Date().toLocaleTimeString([], { hour12: false });
    li.append(t, text);
    li.className = kind;
    feed.append(li);
    while (feed.childElementCount > 200) feed.firstElementChild!.remove();
    feed.scrollTop = feed.scrollHeight;
    log.hidden = false;
}

// ── the diagram: the way the remote computer chose in rdp-host, and where the connection is now ──
//
// One way at a time, its fields only, on the arrows of what goes from person to person: ways 1, 2 and 3 - the line of the machine
// address:port/token comes here; way 4 - the line of this page goes there, its answer comes back ( or a line address:port/token, when the router of the
// machine opened a port ). Each part of the diagram shows its state: waited for, on its way, done, or where it broke. Enter
// connects only when the way has all it needs, and nothing is on its way already.

type Way = 1 | 2 | 3 | 4;
/** Where the connection is: the stage the desk told, or the meeting of way 4. */
type Where = "" | Stage | "meeting" | "met" | "noPath";

/** What the middle of the scene says before anything happens: what each way goes through, in plain words. */
const MIDDLE: Record<Way, string> = {
    1: "",
    2: "straight to the address of its router: the router lets you in",
    3: "a router of yours brings the two together",
    4: "straight through both routers, no one in between",
};

let way: Way = 1;
/** The way the scene is drawn for. */
let drawn: Way | undefined;
/** Connecting, or meeting: Enter waits. */
let busy = false;
/** The offer of this page waiting for its answer: way 4. */
let pending: Offer | undefined;
/** The line of this page being made. */
let making = false;
/** The desk goes over the data channel of way 4. */
let overChannel = false;
let where: Where = "";
/** What the desk or the meeting said last. */
let whereText = "";
const stun = new Map<string, "ask" | "ok" | "no">();
/**
 * Way 4, what is known of the way through the routers: `strict` - the network of this device gives it a new port for each address,
 * the way is in doubt; `doomed` - and the answer says the router of the machine lets in only what it asked for: they will not meet.
 */
let net: "" | "strict" | "doomed" = "";

/** The port the program listens on when its line names none. */
const PORT = 8765;

/** What the line of each way looks like, in its field until one is pasted. */
const EXAMPLE: Record<Way, string> = {
    1: "192.168.1.20:8765/K7QM2XHP9RTA4WZD8CNE-24NDRCK9",
    2: "203.0.113.7:8765/K7QM2XHP9RTA4WZD8CNE-24NDRCK9",
    3: "router.example.org:41933/K7QM2XHP9RTA4WZD8CNE-24NDRCK9",
    4: "rd1a.EQh6cTJmY1M2dFZ4b0pXa3RnU2xh…",
};

/**
 * Where to dial, of what the user has: `192.168.1.20:8765`, `router.example.org`, `[fd00::1]:8765` - the address and the port, all
 * the line of the program says; `ws://` is the page's to add, `wss://` stays when typed, for a machine behind a proxy of TLS.
 * `undefined` - not an address.
 */
function machine(text: string): string | undefined {
    const typed = text.trim(), given = /^wss?:\/\//i.exec(typed)?.[0];
    const scheme = given?.toLowerCase() ?? "ws://", rest = typed.slice(given?.length ?? 0);
    const m = /^(\[[0-9a-f:.]+\]|[^\s:/#[\]]+)(?::(\d{1,5}))?\/?$/i.exec(rest);
    return m === null ? undefined : `${scheme}${m[1]}:${m[2] ?? PORT}`;
}

/**
 * The line of the program, as it printed it and the user pasted it - `<address>:<port>/<token>` -: where to dial and the token. The
 * field keeps it whole, untouched: the page takes it apart only here. `undefined` - not such a line.
 */
function theLine(text = address.value): { at: string; token: string; check: string | undefined } | undefined {
    const line = split(text), at = line === undefined ? undefined : machine(line.address);
    const pass = line?.token.trim() ?? "";
    if (at === undefined || pass === "") return undefined;
    // after the token, `-` and the check of the key of the machine: 8 of the letters of the token; a line without it - the token alone
    const dash = pass.lastIndexOf("-"), check = pass.slice(dash + 1);
    return dash > 0 && /^[A-HJ-NP-Z2-9]{8}$/.test(check) ? { at, token: pass.slice(0, dash), check } : { at, token: pass, check: undefined };
}

/** What the field on the arrow back holds. */
function back(): "empty" | "line" | "noToken" | "answer" | "ours" | "other" {
    const text = address.value.trim();
    if (text === "") return "empty";
    if (card(text, true) !== undefined) return "answer";
    if (card(text, false) !== undefined) return "ours";
    if (theLine() !== undefined) return "line";
    // an address, with nothing after / ( or # of a program of before ): the token is missing
    return machine(/^(?:wss?:\/\/)?[^/#]*/i.exec(text)![0]) !== undefined ? "noToken" : "other";
}

/** Has the way chosen all it needs to connect. */
function ready(): boolean {
    const b = back();
    return b === "line" || (b === "answer" && way === 4 && pending !== undefined);
}

function choose(chosen: Way): void {
    if (busy) return;
    way = chosen;
    for (const b of wayButtons) b.setAttribute("aria-checked", String(b.dataset["way"] === String(chosen)));
    store.set("way", String(chosen));
    where = "";
    draw();
}

for (const b of wayButtons) b.addEventListener("click", () => choose(Number(b.dataset["way"]) as Way));

const set = (part: Part, state: string) => { map.dataset[part] = state; };
function say(element: HTMLElement, text: string, kind: Kind = ""): void {
    element.textContent = text;
    element.classList.remove("ok", "bad", "wait");
    if (kind !== "") element.classList.add(kind);
}
function show(text: string, kind: Kind = ""): void {
    statusLine.textContent = text;
    statusLine.className = kind;
}

/** The diagram as things are now. */
function draw(): void {
    const b = back();
    map.dataset["way"] = String(way);
    if (drawn !== way) { drawScene(scene, way); drawn = way; }
    backNumber.textContent = way === 4 ? "2" : "1";
    backLabel.textContent = way === 4 ? "Its answer comes back: paste it here" : "Its line comes to you: paste it here";
    address.placeholder = EXAMPLE[way]; // what such a line looks like: the user sees what to paste
    // way 4, the two will not meet, or did not: another answer to this line is no use - the field of 2 is shut till a new line
    const shut = way === 4 && (net === "doomed" || where === "noPath");
    map.dataset["shut"] = shut ? "1" : "";
    // way 4: 2 is there only once 1 is done - the line made, or the meeting under way
    map.dataset["made"] = pending !== undefined || where !== "" || overChannel ? "1" : "";
    for (const e of [address, yourName, directStart, ...wayButtons]) e.disabled = busy || (e === directStart && making) || (e === address && shut);
    directStart.textContent = making ? "Asking the STUN servers…" : pending !== undefined ? "Make a new line" : "Make the line";
    directStart.classList.toggle("second", pending !== undefined); // made: what is pressed now is on the other arrow
    offerSaid.hidden = pending === undefined;
    STUN_NAMES.forEach((name, i) => document.getElementById(`stun-${i}`)?.setAttribute("data-s", stun.get(name) ?? ""));

    // before connecting: what is here, what is still waited for
    set(nodeMe, busy ? "try" : ready() ? "ok" : "");
    set(nodeMid, ""); say(midSay, MIDDLE[way]);
    const net4 = way === 4 ? net : "";
    map.dataset["net"] = net4;
    if (net4 === "strict") { set(nodeMid, "wait"); say(midSay, "your network gives a new port for each address: only a router that lets you in meets it", "wait"); }
    if (net4 === "doomed") { set(nodeMid, "bad"); say(midSay, "your network and its router will not meet directly: way 3, or Wi-Fi", "bad"); }
    set(wireA, ""); set(wireB, "");
    set(arrowOut, making ? "try" : pending !== undefined ? "ok" : "wait");
    const waitingBack = way !== 4 || pending !== undefined;
    set(arrowBack, b === "line" || (b === "answer" && pending !== undefined) ? "ok"
        : b === "empty" ? (waitingBack ? "wait" : "") : "bad");
    set(nodeRem, b === "empty" && waitingBack ? "wait" : "");
    say(remSay, b !== "empty" ? "" : way !== 4 ? "sends you its line" : pending !== undefined ? "pastes your line, sends its answer" : "", "wait");
    if (way === 4 && b === "answer" && pending !== undefined) { set(nodeRem, "ok"); say(remSay, "its addresses came with its answer", "ok"); }

    if (where === "") {
        const [text, kind] = hint(b);
        return show(text, kind);
    }

    // connecting: where it is, or where it broke
    set(arrowOut, pending !== undefined || overChannel ? "ok" : ""); set(arrowBack, "ok");
    /** Broke at `wire`: the part before it went, the part after it is not known. */
    const cut = (wire: Part, node: Part, at: HTMLElement, text: string) => {
        set(wireA, wire === wireA ? "cut" : "up");
        set(wireB, wire === wireB ? "cut" : "");
        set(node, "bad"); say(at, text, "bad");
    };
    switch (where) {
        case "meeting":
            set(wireA, "try"); set(wireB, "try"); set(nodeMid, net4 === "doomed" ? "bad" : "try"); set(nodeRem, "try");
            if (net4 === "doomed") say(midSay, "trying anyway: your network and its router will not meet directly", "bad");
            else say(midSay, "trying the pairs of addresses of both sides…");
            say(remSay, "has your line: knocks at your addresses");
            break;
        case "met":
            set(wireA, "up"); set(wireB, "up"); set(nodeMid, "ok"); say(midSay, "the way through both routers is found", "ok");
            break;
        case "noPath":
            set(wireA, "cut"); set(wireB, "cut"); set(nodeMid, "bad"); say(midSay, whereText, "bad");
            break;
        case "dialing":
            if (overChannel) { set(wireA, "up"); set(wireB, "up"); set(nodeMid, "ok"); } else { set(wireA, "try"); set(wireB, "try"); }
            set(nodeRem, "try"); say(remSay, "reaching it…");
            break;
        case "reached":
            set(wireA, "up"); set(wireB, "up"); set(nodeRem, "try"); say(remSay, "answered: proving its key…");
            break;
        case "proved":
            set(wireA, "up"); set(wireB, "up"); set(nodeRem, "try"); say(remSay, "its key is proved: the token goes, waiting to be let in…");
            break;
        case "otherKey":
            set(wireA, "up"); set(wireB, "up"); set(nodeRem, "bad"); say(remSay, "answers with another key than before: is it the same computer?", "bad");
            break;
        case "notTheLine":
            set(wireA, "up"); set(wireB, "up"); set(nodeRem, "bad"); say(remSay, "not the computer of the line: its key does not fit it - the token was not sent", "bad");
            break;
        case "live":
            set(wireA, "up"); set(wireB, "up"); set(nodeMid, "ok"); set(nodeRem, "ok"); say(remSay, "let you in", "ok");
            break;
        case "lost":
            set(wireA, "try"); set(wireB, "try"); set(nodeRem, "wait"); say(remSay, "the connection broke: dialing again…", "wait");
            break;
        case "lostDirect":
            set(wireA, "cut"); set(wireB, "cut"); set(nodeMid, "bad"); say(midSay, "the direct path broke: make a new line", "bad");
            break;
        case "unreachable":
            if (way === 3) cut(wireA, nodeMid, midSay, "the meeting point does not answer, or the remote computer is not at it");
            else cut(wireB, nodeRem, remSay,
                way === 1 ? "not reached at this address: is rdp-host running, are both in one network, does its firewall let it in?"
                : way === 2 ? "not reached: is the port forwarded on its router, and its address public?"
                : "its router did not let you in: make the line in 1 instead");
            break;
        case "noAnswer":
            cut(wireB, nodeRem, remSay, "does not answer any more");
            break;
        case "failed":
            cut(wireB, nodeRem, remSay, whereText);
            break;
        case "refused":
        case "closed":
            set(wireA, "up"); set(wireB, "up"); set(nodeRem, "bad");
            say(remSay, where === "closed" ? "closed the connection while you logged in" : whereText, "bad");
            break;
        case "left":
            break;
    }
}

/** What is waited for now, before connecting: the line under the diagram. */
function hint(b: ReturnType<typeof back>): [string, Kind] {
    if (b === "ours") return ["That is the line of this page: it goes to the remote computer. Paste here what comes back.", "bad"];
    if (b === "other") {
        if (way === 4) return ["That is not an answer: it starts with rd1a., or is a line address:port/token.", "bad"];
        // what in the text is not of a line, by its code: a sign that looks like one and is not shows here
        const odd = [...new Set(address.value.trim().match(/[^\x21-\x7e]/gu) ?? [])].slice(0, 4)
            .map(c => `${c.trim() === "" ? "·" : c} U+${c.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0")}`);
        return [english.badAddress + (odd.length > 0 ? ` It has: ${odd.join(", ")}.` : ""), "bad"];
    }
    if (b === "noToken") return ["The token is missing: paste the whole line as the program printed it, with / and what follows.", "bad"];
    if (b === "answer" && way !== 4) return ["That is the answer of a direct connection: choose 4 above.", "bad"];
    if (b === "answer" && pending === undefined)
        return ["This answer is to a line made before: make a new line in 1, send it, and paste the answer that comes back.", "bad"];
    if (ready()) return ["All is here: press Enter.", "ok"];
    if (way !== 4) return ["Waiting for the line of the remote computer: when it comes by chat or mail, paste it on the arrow.", "wait"];
    if (making) return ["Asking the STUN servers the address of this computer…", ""];
    if (pending === undefined)
        return ["Start with 1: make the line of this page. The remote computer sent a line address:port/token? That is way 2: choose 2 above.", "wait"];
    if (pending.strict)
        return ["Your line is in the clipboard. This network gives a new port for each address: if rdp-host says its router lets in only " +
            "what it asked for, they will not meet - use way 3, or this device on Wi-Fi.", "wait"];
    return ["Your line is in the clipboard: send it. Now waiting for its answer, rd1a.…", "wait"];
}

// ── the session ──

/** The desk being connected from the form: what it tells is drawn on the diagram until it is in. */
let attempting: Open | undefined;
/** Where a desk waits, out of sight, until its machine lets the page in: then it goes into a panel. */
const waiting = document.createElement("div");
waiting.hidden = true;
document.body.append(waiting);

/** Connects, when all is there: as soon as the line is pasted; a line typed by hand at Enter, in its field or the one of the name. */
function go(): void {
    if (busy || !ready()) return;
    const answer = card(address.value, true);
    if (answer !== undefined) return void meet(answer);
    const line = theLine()!;
    openDesk(line.at, line.token, undefined, undefined, line.check, { line: address.value.trim(), way });
}

form.addEventListener("submit", e => { e.preventDefault(); go(); });
// two fields and no button: a browser does not submit the form on Enter by itself then
for (const field of [address, yourName]) field.addEventListener("keydown", e => {
    if (e.key !== "Enter" || e.isComposing) return;
    e.preventDefault();
    go();
});

/** What the desk tells of each stage, in the log. */
const STAGE_NOTE: Partial<Record<Stage, string>> = {
    reached: "The remote computer answered: the handshake of Noise goes, it proves its key",
    proved: "Its key is proved: the token goes, waiting to be let in",
    live: "Let in: its screen comes",
    left: "Disconnected",
};

/** The desk on the machine `at`, by `typed` - the token -; direct: over `channel`, `attempt` closed with the desk. `from` - the line it
 *  came of, kept among the lines once the machine lets the page in. */
function openDesk(at: string, typed: string, channel?: RTCDataChannel, attempt?: Offer, check?: string, from?: { line: string; way: Way }): void {
    store.set("name", yourName.value);
    busy = true;
    overChannel = channel !== undefined;
    forget.hidden = true;
    keyLine.textContent = "";
    note(channel === undefined ? `Dialing ${at}` : "The session starts over the direct channel");

    // connecting now: an attempt of the form before it, not in yet, ends; the machines in stay
    attempting?.desk.leave();
    const o: Open = { desk: undefined as unknown as Desk, title: at.replace(/^ws:\/\//, ""), panel: undefined, window: undefined, ended: false };
    const mine = () => attempting === o;
    attempting = o;
    opens.push(o);
    o.desk = open(waiting, {
        address: at, token: typed, check, name: yourName.value, store, channel,
        onStage: s => {
            if (!mine()) return;
            where = s === "left" ? "" : s;
            const said = STAGE_NOTE[s];
            if (said !== undefined) note(said, s === "live" ? "ok" : "");
            draw();
        },
        onStatus: (text, bad) => {
            if (!mine()) return;
            whereText = text;
            if (bad) note(text, "bad");
            draw();
            show(text, bad ? "bad" : "");
        },
        onKey: (text, other) => {
            if (!mine()) return;
            keyLine.textContent = other ? text : ""; // the key said only when it is not the one kept: else the page checks it alone
            forget.hidden = !other;
            if (text !== "") note(text, other ? "bad" : "");
        },
        onLive: (on, host) => {
            o.panel?.api.setTitle(on ? o.title : `⟳ ${o.title}`); // broke: it dials again on its own, the desk says how it goes
            if (!on || o.panel || o.window) return;
            // in: a panel of its own, in front; the form is free for another machine
            if (host !== "") o.title = `${host} · ${o.title}`;
            if (from !== undefined) lineConnected(from.line, from.way, host);
            attempting = undefined;
            busy = false;
            overChannel = false;
            panelOf(o);
            showForm(false);
        },
        onEnded: (text, bad) => {
            attempt?.close();
            o.ended = true;
            opens.splice(opens.indexOf(o), 1);
            if (o.panel) dock.removePanel(o.panel);
            o.window?.close();
            if (bad) note(`${o.title}: ${text}`, "bad");
            if (mine()) {
                // never in: the form says why, the diagram where
                attempting = undefined;
                busy = false;
                overChannel = false;
                whereText = text;
                draw();
                if (where !== "") show(text, bad ? "bad" : "");
                return seenAll();
            }
            // a machine that was in: the form, saying why it ended, when no other is left
            if (!opens.some(x => x.panel || x.window)) {
                showForm(true);
                show(`${o.title}: ${text}`, bad ? "bad" : "");
            } else seenAll();
        },
    });
    draw();
}

// ── way 4, direct: 1 makes the line of this page, it goes to the user of the machine by hand; 2 takes its answer ──
//
// The line is not shown: nobody reads it, it is pasted. It goes into the clipboard as soon as it is made; when the browser does not let
// the page in there without a press, the button puts it there - a press always may. The textarea out of sight holds it for the old
// way of copying, where the clipboard of the browser is missing. On the diagram the STUN servers light up as they answer; the log
// tells what each one saw.

/** What the making of the line and the meeting tell: the diagram and the log. */
function heard(h: Heard): void {
    switch (h.k) {
        case "ask":
            stun.clear();
            for (const s of h.servers) stun.set(s, "ask");
            note(`Asking ${h.servers.length} STUN servers at once: ${h.servers.join(", ")}`);
            break;
        case "local": note(`This device itself: ${h.address}`); break;
        case "seen":
            stun.set(h.server, "ok");
            note(`${h.server} answered in ${h.ms} ms: the internet sees this device at ${h.address}`, "ok");
            break;
        case "silent":
            stun.set(h.server, "no");
            note(`${h.server}: ${h.why}`, "bad");
            break;
        case "gathered": note(`The line is made in ${h.ms} ms: ${h.seen} of ${STUN_NAMES.length} STUN servers answered`, h.seen > 0 ? "ok" : "bad"); break;
        case "strict":
            note("The network of this device gives it a new port for each address it talks to ( a strict NAT, as mobile operators have ): " +
                "it meets the remote computer only if the router there lets in what it did not ask for. If not: way 3, or this device on Wi-Fi", "wait");
            break;
        case "state": note(`ICE: ${h.state}`, h.state === "failed" ? "bad" : h.state === "connected" ? "ok" : ""); break;
        case "checked": note(`${h.address}: ${h.sent} checks sent, ${h.answered} answered`, h.answered > 0 ? "" : "bad"); break;
        case "met":
            if (where === "meeting") where = "met";
            note(`The way is found in ${h.ms} ms: ${h.mine} ↔ ${h.theirs}, ${h.how === "network" ? "inside one network" : "through the internet"}`, "ok");
            break;
    }
    draw();
}

/** Back to 1: no line made. */
function unmade(): void {
    pending?.close();
    pending = undefined;
    net = "";
}

directStart.addEventListener("click", () => void (async () => {
    unmade();
    if (card(address.value, true) !== undefined) address.value = ""; // the answer to the line before: no use to the new one
    making = true;
    where = "";
    draw();
    try {
        pending = await offer(heard);
        net = pending.strict ? "strict" : "";
        offerLine.value = pending.line;
        await putLine();
    } catch (e) {
        const why = e instanceof Error ? e.message : String(e);
        note("The line is not made: " + why, "bad");
        show("Direct connection: " + why, "bad");
    } finally {
        making = false;
        draw();
    }
})());

offerCopy.addEventListener("click", () => void putLine());

/** The line of this page put in the clipboard, and said so; or what to press when the browser did not let it in. */
async function putLine(): Promise<void> {
    const put = await toClipboard(offerLine.value);
    offerSaid.classList.toggle("bad", !put);
    offerTold.textContent = put
        ? "✓ It is in your clipboard: send it."
        : "The browser did not let the page into the clipboard: press";
    offerCopy.textContent = put ? "Copy again" : "Put it in the clipboard";
}

async function toClipboard(text: string): Promise<boolean> {
    try {
        await navigator.clipboard.writeText(text);
        return true;
    } catch {
        offerLine.value = text;
        offerLine.select();
        return document.execCommand("copy");
    }
}

/** The answer of the machine came: the two meet, the desk goes over the data channel. */
async function meet(answer: Card): Promise<void> {
    const attempt = pending;
    if (attempt === undefined) return;
    pending = undefined; // the attempt is the desk's now: unmade() does not close it
    busy = true;
    where = "meeting";
    note(`The answer has ${answer.candidates.length} addresses of the remote computer: ${answer.candidates.map(c => c.ip + ":" + c.port).join(", ")}`);
    // both told: this network changes the port for each address, the router there lets in only what it asked for - ICE still tries
    const doomed = attempt.strict && answer.picky
        ? "this device and the remote computer will not meet directly: the network of this device gives it a new port for each address " +
          "( as mobile operators do ), and the router of the remote computer lets in only what it asked for. Use way 3, or this device on Wi-Fi"
        : undefined;
    if (doomed !== undefined) { net = "doomed"; note(doomed, "bad"); }
    note("Meeting: ICE tries the addresses of both sides, pair by pair, a minute at most");
    draw();
    show(doomed !== undefined ? "Direct connection: " + doomed : "Meeting the remote computer: trying the ways through both routers…", doomed !== undefined ? "bad" : undefined);
    let channel: RTCDataChannel;
    try {
        channel = await attempt.finish(answer);
    } catch (e) {
        attempt.close();
        busy = false;
        where = "noPath";
        whereText = "no direct path: make a new line, or use way 3";
        const why = e instanceof Error ? e.message : String(e);
        note(why, "bad");
        draw();
        return show("Direct connection: " + why, "bad");
    }
    address.value = "";
    openDesk("direct", answer.token, channel, attempt);
}

forget.addEventListener("click", () => {
    const line = theLine();
    if (line !== undefined) forgetKey(store, line.at);
    forget.hidden = true;
    keyLine.textContent = english.keyForgotten;
});

// ── the computers kept: a name the user gave each, and its line - read by the camera, or connected with ──
//
// One row of the store each, newest first: when, the way, the name the machine told, the name the user gave, the line - apart by tabs; a
// line of the program has neither tab nor end of line in it, and a name loses them. The list shows the names: a press on one connects;
// Copy and Send take its line to another device, the computer of the user. A line read by the camera is named: a new name adds a
// computer, a name there already gives that computer the new line - a new start of rdp-host, a new port or token.

/** A computer kept. */
interface Kept { time: number; way: Way; host: string; name: string; line: string }
/** How many are kept: the oldest goes. */
const KEPT = 20;

function keptLines(): Kept[] {
    const all: Kept[] = [];
    for (const row of (store.get("lines") ?? "").split("\n")) {
        const [time, w, host, name, line] = row.split("\t");
        const n = Number(w);
        if (line !== undefined && line !== "" && n >= 1 && n <= 4) all.push({ time: Number(time) || 0, way: n as Way, host: host ?? "", name: name ?? "", line });
    }
    return all;
}

const clean = (text: string) => text.replace(/[\t\n\r]+/g, " ").trim();

function storeLines(all: Kept[]): void {
    store.set("lines", all.length === 0 ? undefined : all.slice(0, KEPT).map(k => `${k.time}\t${k.way}\t${clean(k.host)}\t${clean(k.name)}\t${k.line}`).join("\n"));
    drawLines();
}

/** Its name as the list shows it: the one the user gave; else the one the machine told; else its address. */
const nameOf = (k: Kept) => k.name || k.host || (split(k.line)?.address ?? k.line);

/** `line` under `name`, first: the computer of that name gets it, or a new one is added; `was` - the one being renamed, it goes. */
function saveLine(name: string, line: string, w: Way, was?: Kept): Kept {
    const all = keptLines(), same = all.find(k => nameOf(k) === name);
    const kept: Kept = { time: Date.now(), way: w, host: same?.host ?? was?.host ?? "", name, line };
    storeLines([kept, ...all.filter(k => k !== same && !(was !== undefined && k.line === was.line && nameOf(k) === nameOf(was)))]);
    return kept;
}

/** The machine of `line` let the page in: the computer of that line comes first, with the name the machine told; a line not kept yet
 *  is kept, by that name. */
function lineConnected(line: string, w: Way, host: string): void {
    const all = keptLines(), same = all.find(k => k.line === line);
    const kept: Kept = same !== undefined ? { ...same, time: Date.now(), host: host || same.host } : { time: Date.now(), way: w, host, name: "", line };
    storeLines([kept, ...all.filter(k => k !== same)]);
}

/** Copied, or the browser said why not: said on the button a moment. */
async function copyLine(line: string, button: HTMLButtonElement): Promise<void> {
    try {
        await navigator.clipboard.writeText(line);
        button.textContent = "Copied ✓";
    } catch {
        offerLine.value = line; // the old way: a selection, copied
        offerLine.select();
        button.textContent = document.execCommand("copy") ? "Copied ✓" : "Not copied";
    }
    setTimeout(() => { button.textContent = "Copy"; }, 2000);
}

function small(text: string, title: string, act: () => void, className = "second"): HTMLButtonElement {
    const b = document.createElement("button");
    b.type = "button";
    b.className = className;
    b.textContent = text;
    b.title = title;
    b.addEventListener("click", act);
    return b;
}

function drawLines(fresh?: string): void {
    const all = keptLines();
    linesBox.hidden = all.length === 0;
    linesList.replaceChildren(...all.map(k => {
        const li = document.createElement("li");
        if (k.line === fresh) li.className = "new";
        const use = document.createElement("button");
        use.type = "button";
        use.className = "use";
        use.title = "Connect";
        const name = document.createElement("b");
        name.textContent = nameOf(k);
        const when = document.createElement("small");
        when.textContent = [k.name !== "" && k.host !== "" ? k.host : "", `way ${k.way}`,
            new Date(k.time).toLocaleString(undefined, { dateStyle: "short", timeStyle: "short" })].filter(x => x !== "").join(" · ");
        use.append(name, when);
        // the line itself only when asked: the list is of names
        const text = document.createElement("code");
        text.textContent = k.line;
        use.addEventListener("click", () => {
            if (busy) return;
            choose(k.way);
            address.value = k.line;
            draw();
            go();
        });
        const copy: HTMLButtonElement = small("Copy", "Its line, into the clipboard", () => void copyLine(k.line, copy));
        const parts: HTMLElement[] = [use];
        parts.push(small("📷 Update", "Read the QR code rdp-host shows now: this computer gets the new line", () => void update(k)));
        parts.push(small("Line", "Show its line, or hide it", () => li.classList.toggle("open")), copy);
        if (typeof navigator.share === "function")
            parts.push(small("Send", "Its line to a messenger or to mail: open it on your computer and paste it into the page there",
                () => void navigator.share({ text: k.line }).catch(() => undefined)));
        parts.push(small("✎", "Another name", () => nameLine(k.line, k.way, k), "plain edit"),
            small("✕", "Forget this computer", () => storeLines(keptLines().filter(x => x.line !== k.line || nameOf(x) !== nameOf(k))), "plain drop"));
        const row = document.createElement("div");
        row.className = "row";
        row.append(...parts);
        li.append(row, text);
        return li;
    }));
}

// the window that names a line: a new name, or one of those there - that computer gets the line
const naming = $<HTMLDialogElement>("naming");
const namingLine = $<HTMLElement>("naming-line"), namingName = $<HTMLInputElement>("naming-name");
const namingNames = $<HTMLDivElement>("naming-names"), namingSays = $<HTMLParagraphElement>("naming-says");
const namingSave = $<HTMLButtonElement>("naming-save"), namingGo = $<HTMLButtonElement>("naming-go");
/** What the window names now: the line, its way, the computer renamed. */
let named: { line: string; way: Way; was: Kept | undefined } | undefined;

/** What saving does, said under the name: a new computer, or the new line of one there. */
function namingSaid(): void {
    const name = clean(namingName.value), was = named?.was;
    const same = keptLines().find(k => nameOf(k) === name);
    namingSave.disabled = namingGo.disabled = name === "";
    for (const b of namingNames.querySelectorAll("button")) b.classList.toggle("on", b.textContent === name);
    namingSays.textContent = name === "" ? "Give it a name, or press one you have"
        : same === undefined ? (was !== undefined ? "The computer gets this name" : "A new computer in your list")
        : same.line === named?.line ? "The same computer, the same line"
        : `“${name}” gets this line in place of its line before`;
}

/** Opens the window for `line`: named by the user, or renamed - `was`. */
function nameLine(line: string, w: Way, was?: Kept): void {
    named = { line, way: w, was };
    namingLine.textContent = line;
    const all = keptLines();
    // the computer this line is likely of: the same address, a new port or token of the program there
    const at = split(line)?.address.replace(/:\d+$/, "");
    const likely = was ?? all.find(k => k.line === line) ?? all.find(k => split(k.line)?.address.replace(/:\d+$/, "") === at);
    namingName.value = likely !== undefined ? nameOf(likely) : "";
    namingNames.replaceChildren(...[...new Set(all.map(nameOf))].map(n => small(n, "This computer gets the line", () => { namingName.value = n; namingSaid(); }, "chip")));
    namingNames.hidden = all.length === 0 || was !== undefined;
    namingGo.hidden = was !== undefined;
    namingSaid();
    naming.showModal();
    namingName.focus();
}

namingName.addEventListener("input", namingSaid);
naming.addEventListener("close", () => {
    const n = named, how = naming.returnValue, name = clean(namingName.value);
    named = undefined;
    if (n === undefined || name === "" || (how !== "save" && how !== "go")) return;
    const kept = saveLine(name, n.line, n.way, n.was);
    drawLines(kept.line);
    if (how === "go") {
        if (busy) return;
        choose(kept.way);
        address.value = kept.line;
        draw();
        go();
    } else if (n.was === undefined) {
        show(`✓ “${name}” is in your list above: press it to connect, or copy or send its line to your computer.`, "ok");
        linesBox.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
});

/** The line of `k` anew, read by the camera: rdp-host was started again, a new port or token. */
async function update(k: Kept): Promise<void> {
    const text = await read();
    if (text === undefined) return;
    if (card(text, true) !== undefined) return show("That QR code is the answer of way 4 for this meeting, not a line of a computer to keep.", "bad");
    if (theLine(text) === undefined) return show(`That QR code holds no line of rdp-host: ${text.slice(0, 80)}`, "bad");
    note(`Read by the camera, for “${nameOf(k)}”: ${text}`, "ok");
    const kept = saveLine(nameOf(k), text, k.way, k);
    drawLines(kept.line);
    show(`✓ “${nameOf(k)}” has its new line: press it to connect.`, "ok");
}

/** A QR code read by the camera; undefined - cancelled, or no camera: said. */
async function read(): Promise<string | undefined> {
    try {
        return await scan();
    } catch (e) {
        show(`The camera: ${(e as Error).message}`, "bad");
        return undefined;
    }
}

// the camera: the QR code rdp-host shows. A line is named and kept, one press from connecting; the answer of way 4 is for this meeting
// only - it goes into the field, as if pasted
scanButton.hidden = false;
scanButton.addEventListener("click", async () => {
    const text = await read();
    if (text === undefined) return;
    if (card(text, true) !== undefined) {
        address.value = text;
        address.dispatchEvent(new InputEvent("input", { inputType: "insertFromPaste" }));
        return;
    }
    if (theLine(text) === undefined) {
        show(`That QR code holds no line of rdp-host: ${text.slice(0, 80)}`, "bad");
        return;
    }
    note(`Read by the camera: ${text}`, "ok");
    nameLine(text, way);
});
drawLines();

address.addEventListener("input", e => {
    where = ""; // something new typed: what broke before is not this attempt's
    if (card(address.value, true) !== undefined && way !== 4) choose(4); // the answer of way 4: only way 4 takes it
    draw();
    // pasted, and all is there: it connects at once - nothing more to press; a line typed by hand waits for Enter
    if ((e as InputEvent).inputType === "insertFromPaste" || (e as InputEvent).inputType === "insertFromDrop") go();
});

// what the page knew last time: the name and the way; not the line - its token is new at each start of the program
store.set("address", undefined);
yourName.value = store.get("name") ?? "";
const kept = Number(store.get("way"));
choose(kept >= 1 && kept <= 4 ? kept as Way : 1);

// tokens kept by the page before: none is kept now
try {
    for (let i = localStorage.length - 1; i >= 0; i--) if (localStorage.key(i)?.startsWith("rdp.token.")) localStorage.removeItem(localStorage.key(i)!);
} catch { /* a private window */ }
