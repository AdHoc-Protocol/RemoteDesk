// The viewer page of RemoteDesk: the login form, and the desk of the session ( ./desk.ts - the bar, the screen, the session in its
// worker ) put in place of the form while the machine has the page in. What the page keeps is kept on this device, in localStorage: the
// address and the name typed last, what the desk keeps; the token is not kept - it lives in the desk, for its session only.

import { english, forgetKey, open, splitLine as split, type Desk, type Stage, type Store } from "./desk";
import { card, offer, STUN_NAMES, type Card, type Heard, type Offer } from "./direct";
import { drawScene } from "./scene";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/** The hand the diagram is written in: the font, as data, put in by build.mjs. */
declare const HAND_FONT: string;
const hand = document.createElement("style");
hand.textContent = `@font-face { font-family: "RD Hand"; src: url(${HAND_FONT}) format("woff2"); font-weight: 400 700; font-display: block; }`;
document.head.append(hand);

const form = $<HTMLFormElement>("login");
const address = $<HTMLInputElement>("address");
const token = $<HTMLInputElement>("token");
const yourName = $<HTMLInputElement>("name");
const connectButton = $<HTMLButtonElement>("connect");
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

// ── what the page keeps on this device ──

const store: Store = {
    get(name: string): string | undefined { try { return localStorage.getItem("rdp." + name) ?? undefined; } catch { return undefined; } },
    set(name: string, value: string | undefined): void {
        try { value === undefined ? localStorage.removeItem("rdp." + name) : localStorage.setItem("rdp." + name, value); } catch { /* a private window */ }
    },
};

type Kind = "" | "bad" | "ok" | "wait";

/** The desk shown while the machine has the page in, the form while not. */
function seen(desk: boolean): void {
    form.style.display = desk ? "none" : "";
    deskHost.classList.toggle("on", desk);
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
// ws://…#token comes here; way 4 - the line of this page goes there, its answer comes back ( or a line ws://…, when the router of the
// machine opened a port ). Each part of the diagram shows its state: waited for, on its way, done, or where it broke. Connect is
// pressable only when the way has all it needs, and nothing is on its way already.

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
/** Connecting, or meeting: Connect waits. */
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

/** What the field on the arrow back holds. */
function back(): "empty" | "line" | "noToken" | "answer" | "ours" | "other" {
    const text = address.value.trim();
    if (text === "") return "empty";
    if (card(text, true) !== undefined) return "answer";
    if (card(text, false) !== undefined) return "ours";
    if (!/^wss?:\/\/./.test(text)) return "other";
    return token.value === "" ? "noToken" : "line";
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
    address.placeholder = way === 4 ? "rd1a.…  ( or a line ws://…#… it sent )" : "ws://…#…";
    for (const e of [address, token, yourName, directStart, ...wayButtons]) e.disabled = busy || (e === directStart && making);
    connectButton.disabled = busy || !ready();
    directStart.textContent = making ? "Asking the STUN servers…" : pending !== undefined ? "Make a new line" : "Make the line";
    directStart.classList.toggle("second", pending !== undefined); // made: what is pressed now is on the other arrow
    offerSaid.hidden = pending === undefined;
    STUN_NAMES.forEach((name, i) => document.getElementById(`stun-${i}`)?.setAttribute("data-s", stun.get(name) ?? ""));

    // before connecting: what is here, what is still waited for
    set(nodeMe, busy ? "try" : ready() ? "ok" : "");
    set(nodeMid, ""); say(midSay, MIDDLE[way]);
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
            set(wireA, "try"); set(wireB, "try"); set(nodeMid, "try"); set(nodeRem, "try");
            say(midSay, "trying the pairs of addresses of both sides…");
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
    if (b === "other") return [way === 4 ? "That is not an answer: it starts with rd1a., or ws:// for a line." : english.badAddress, "bad"];
    if (b === "noToken") return ["The page keeps no token: paste the whole line, ws://…#…, the token comes after #.", "wait"];
    if (b === "answer" && way !== 4) return ["That is the answer of a direct connection: choose 4 above.", "bad"];
    if (b === "answer" && pending === undefined)
        return ["This answer is to a line made before: make a new line in 1, send it, and paste the answer that comes back.", "bad"];
    if (ready()) return ["All is here: press Connect.", "ok"];
    if (way !== 4) return ["Waiting for the line of the remote computer: when it comes by chat or mail, paste it on the arrow.", "wait"];
    if (making) return ["Asking the STUN servers the address of this computer…", ""];
    if (pending === undefined) return ["Start with 1: make the line of this page. A line ws://… came from the remote computer already? Paste it in 2.", "wait"];
    return ["Your line is in the clipboard: send it. Now waiting for its answer, rd1a.…", "wait"];
}

// ── the session ──

/** The desk open: undefined - none. */
let desk: Desk | undefined;
/** The number of the desk opened last: what an earlier one tells is not heard. */
let opened = 0;

form.addEventListener("submit", e => {
    e.preventDefault();
    if (busy || !ready()) return;
    const answer = card(address.value, true);
    if (answer !== undefined) return void meet(answer);
    splitLine();
    const at = address.value.trim();
    store.set("address", at);
    openDesk(at, token.value);
});

/** What the desk tells of each stage, in the log. */
const STAGE_NOTE: Partial<Record<Stage, string>> = {
    reached: "The remote computer answered: the handshake of Noise goes, it proves its key",
    proved: "Its key is proved: the token goes, waiting to be let in",
    live: "Let in: its screen comes",
    left: "Disconnected",
};

/** The desk on the machine `at`, by `typed` - the token -; direct: over `channel`, `attempt` closed with the desk. */
function openDesk(at: string, typed: string, channel?: RTCDataChannel, attempt?: Offer): void {
    store.set("name", yourName.value);
    busy = true;
    overChannel = channel !== undefined;
    forget.hidden = true;
    keyLine.textContent = "";
    note(channel === undefined ? `Dialing ${at}` : "The session starts over the direct channel");

    const mine = ++opened; // connecting now: the desk before, retrying or not, ends
    desk?.leave();
    desk = open(deskHost, {
        address: at, token: typed, name: yourName.value, store, channel,
        onStage: s => {
            if (mine !== opened) return;
            where = s === "left" ? "" : s;
            const said = STAGE_NOTE[s];
            if (said !== undefined) note(said, s === "live" ? "ok" : "");
            draw();
        },
        onStatus: (text, bad) => {
            if (mine !== opened) return;
            whereText = text;
            if (bad) note(text, "bad");
            draw();
            show(text, bad ? "bad" : "");
        },
        onKey: (text, other) => {
            if (mine !== opened) return;
            keyLine.textContent = text;
            forget.hidden = !other;
            if (text !== "") note(text, other ? "bad" : "");
        },
        onLive: on => {
            if (mine !== opened) return;
            seen(on);
            if (!on) { busy = false; draw(); show(whereText, "bad"); } // the button connects at once, not waiting for the next attempt
        },
        onEnded: (text, bad) => {
            attempt?.close();
            if (mine !== opened) return;
            desk = undefined;
            seen(false);
            busy = false;
            overChannel = false;
            whereText = text;
            if (bad) note(text, "bad");
            draw();
            if (where !== "") show(text, bad ? "bad" : "");
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
        case "state": note(`ICE: ${h.state}`, h.state === "failed" ? "bad" : h.state === "connected" ? "ok" : ""); break;
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
}

directStart.addEventListener("click", () => void (async () => {
    unmade();
    making = true;
    where = "";
    draw();
    try {
        pending = await offer(heard);
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
    note("Meeting: ICE tries the addresses of both sides, pair by pair, a minute at most");
    draw();
    show("Meeting the remote computer: trying the ways through both routers…");
    let channel: RTCDataChannel;
    try {
        channel = await attempt.finish(answer);
    } catch (e) {
        attempt.close();
        busy = false;
        where = "noPath";
        whereText = (e instanceof Error ? e.message : String(e)) + ": make a new line, or use way 3";
        note(whereText, "bad");
        draw();
        return show("Direct connection: " + whereText, "bad");
    }
    address.value = "";
    openDesk("direct", answer.token, channel, attempt);
}

forget.addEventListener("click", () => {
    forgetKey(store, address.value);
    forget.hidden = true;
    keyLine.textContent = english.keyForgotten;
});

/** The line the machine prints for its viewer - `ws://<router>:<port>#<token>` - goes into its two fields: true, it was one. */
function splitLine(): boolean {
    const line = split(address.value);
    if (line === undefined) return false;
    address.value = line.address;
    token.value = line.token;
    return true;
}

address.addEventListener("input", () => {
    where = ""; // something new typed: what broke before is not this attempt's
    if (card(address.value, true) !== undefined) {
        if (way !== 4) choose(4); // the answer of way 4: only way 4 takes it
    } else splitLine();
    draw();
});
token.addEventListener("input", () => { where = ""; draw(); });

// what the page knew last time
address.value = store.get("address") ?? "";
yourName.value = store.get("name") ?? "";
const kept = Number(store.get("way"));
choose(kept >= 1 && kept <= 4 ? kept as Way : 1);

// tokens kept by the page before: none is kept now
try {
    for (let i = localStorage.length - 1; i >= 0; i--) if (localStorage.key(i)?.startsWith("rdp.token.")) localStorage.removeItem(localStorage.key(i)!);
} catch { /* a private window */ }
