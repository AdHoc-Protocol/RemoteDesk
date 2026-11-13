// The viewer page of RemoteDesk: the login form, and the desk of the session ( ./desk.ts - the bar, the screen, the session in its
// worker ) put in place of the form while the machine has the page in. What the page keeps is kept on this device, in localStorage: the
// address and the name typed last, what the desk keeps; the token is not kept - it lives in the desk, for its session only.

import { english, forgetKey, open, splitLine as split, type Desk, type Store } from "./desk";
import { card, offer, type Card, type Offer } from "./direct";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const form = $<HTMLFormElement>("login");
const address = $<HTMLInputElement>("address");
const token = $<HTMLInputElement>("token");
const yourName = $<HTMLInputElement>("name");
const connectButton = $<HTMLButtonElement>("connect");
const statusLine = $<HTMLDivElement>("status");
const keyLine = $<HTMLDivElement>("key");
const forget = $<HTMLButtonElement>("forget");
const deskHost = $<HTMLDivElement>("desk");
const directStart = $<HTMLButtonElement>("direct-start");
const offerBox = $<HTMLDivElement>("offer");
const offerLine = $<HTMLTextAreaElement>("offer-line");
const offerCopy = $<HTMLButtonElement>("offer-copy");

// ── what the page keeps on this device ──

const store: Store = {
    get(name: string): string | undefined { try { return localStorage.getItem("rdp." + name) ?? undefined; } catch { return undefined; } },
    set(name: string, value: string | undefined): void {
        try { value === undefined ? localStorage.removeItem("rdp." + name) : localStorage.setItem("rdp." + name, value); } catch { /* a private window */ }
    },
};

function show(text: string, kind: "" | "bad" | "ok" = ""): void {
    statusLine.textContent = text;
    statusLine.className = kind;
}

/** The desk shown while the machine has the page in, the form while not. */
function seen(desk: boolean): void {
    form.style.display = desk ? "none" : "";
    deskHost.classList.toggle("on", desk);
}

// ── the session ──

/** The desk open: undefined - none. */
let desk: Desk | undefined;
/** The number of the desk opened last: what an earlier one tells is not heard. */
let opened = 0;

form.addEventListener("submit", e => {
    e.preventDefault();
    const answer = card(address.value, true); // way 4: the answer of the machine to the line of this page
    if (answer !== undefined) return void meet(answer);
    splitLine();
    const where = address.value.trim(), typed = token.value;
    if (!/^wss?:\/\//.test(where)) return show(english.badAddress, "bad");
    if (typed === "") return show(english.noToken, "bad");

    store.set("address", where);
    openDesk(where, typed);
});

/** The desk on the machine of `where`, by `typed` - the token -; direct: over `channel`, `attempt` closed with the desk. */
function openDesk(where: string, typed: string, channel?: RTCDataChannel, attempt?: Offer): void {
    store.set("name", yourName.value);
    connectButton.disabled = true;
    forget.hidden = true;
    keyLine.textContent = "";

    const mine = ++opened; // connecting now: the desk before, retrying or not, ends
    desk?.leave();
    desk = open(deskHost, {
        address: where, token: typed, name: yourName.value, store, channel,
        onStatus: (text, bad) => { if (mine === opened) show(text, bad ? "bad" : ""); },
        onKey: (text, other) => {
            if (mine !== opened) return;
            keyLine.textContent = text;
            forget.hidden = !other;
        },
        onLive: on => {
            if (mine !== opened) return;
            seen(on);
            if (!on) connectButton.disabled = false; // the button connects at once, not waiting for the next attempt
        },
        onEnded: (text, bad) => {
            attempt?.close();
            if (mine !== opened) return;
            desk = undefined;
            seen(false);
            connectButton.disabled = false;
            show(text, bad ? "bad" : "");
        },
    });
}

// ── way 4, direct: the line of this page goes to the user of the machine by hand, its answer comes back into Remote machine ──
//
// The line is not shown: nobody reads it, it is pasted. It goes into the clipboard as soon as it is made; when the browser does not let
// the page in there without a press, the button puts it there - a press always may. The textarea out of sight holds it for the old
// way of copying, where the clipboard of the browser is missing.

/** The offer of this page waiting for its answer. */
let pending: Offer | undefined;

directStart.addEventListener("click", () => void (async () => {
    pending?.close();
    pending = undefined;
    directStart.disabled = true;
    show("Finding the addresses of this device…");
    try {
        pending = await offer();
        offerLine.value = pending.line;
        offerBox.hidden = false;
        await putLine();
    } catch (e) {
        show("Direct connection: " + (e instanceof Error ? e.message : String(e)), "bad");
    } finally {
        directStart.disabled = false;
    }
})());

offerCopy.addEventListener("click", () => void putLine());

/** The line of this page put in the clipboard, and said so; or what to press when the browser did not let it in. */
async function putLine(): Promise<void> {
    if (await toClipboard(offerLine.value)) show("The line of this page is in the clipboard: send it to the user of the remote computer.", "ok");
    else show("The browser did not let the page into the clipboard: press Put the line in the clipboard.", "bad");
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
    if (attempt === undefined) return show("First press Direct connection: the answer goes with the line of this page.", "bad");
    pending = undefined;
    connectButton.disabled = true;
    show("Meeting the remote computer…");
    let channel: RTCDataChannel;
    try {
        channel = await attempt.finish(answer);
    } catch (e) {
        attempt.close();
        connectButton.disabled = false;
        return show("Direct connection: " + (e instanceof Error ? e.message : String(e)) + ". Make a new one, or use another way.", "bad");
    }
    offerBox.hidden = true;
    address.value = store.get("address") ?? "";
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

address.addEventListener("input", splitLine);

// what the page knew last time
address.value = store.get("address") ?? address.value;
yourName.value = store.get("name") ?? "";

// tokens kept by the page before: none is kept now
try {
    for (let i = localStorage.length - 1; i >= 0; i--) if (localStorage.key(i)?.startsWith("rdp.token.")) localStorage.removeItem(localStorage.key(i)!);
} catch { /* a private window */ }
