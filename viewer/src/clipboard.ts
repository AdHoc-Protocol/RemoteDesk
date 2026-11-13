// The clipboard of the page and the machine: text the machine copied is set on the clipboard of this device; text copied here goes
// to the worker of the session ( ./worker/clipboard.ts, which sends it when it is new ) with a paste - Ctrl ( ⌘ ) + V, the text of
// the paste itself ( ./input.ts ), nothing asked. Read ahead, when the page gets the focus or the canvas is clicked, only where the
// browser lets the page read the clipboard already: never asked for - a page opened from a file would be asked at each click. Of
// several desks on a page only the one in use ( `active` ) does it: text another machine copied waits until its desk is used.

import { W, type ToWorker } from "./messages";

type Post = (message: ToWorker) => void;
/** The clipboard of the browser: the name Clipboard is the one of this module. */
type Clipboard_ = globalThis.Clipboard;

export interface Clipboard {
    /** Text the machine copied. */
    set(text: string): void;
    /** The desk is the one in use now: what waits goes. */
    sync(): void;
    /** The desk went into another window: the clipboard is the one of that window. */
    moved(): void;
    stop(): void;
}

/** `active` - the desk of this clipboard is the one in use: it has the focus of the page. */
export function start(canvas: HTMLCanvasElement, active: () => boolean, post: Post): Clipboard {
    /** The clipboard of the window the desk is in: the browser lets only the window with the focus use it. */
    const board = () => (canvas.ownerDocument.defaultView ?? window).navigator.clipboard as Clipboard_ | undefined;
    /** Text of the machine the browser did not let the page set yet: set when the page gets the focus. */
    let pending: string | undefined;
    let reading = false;
    let stopped = false; // a read that ends after the stop is not sent
    /** The browser lets the page read the clipboard, not asking: read ahead then. */
    let allowed = false;
    navigator.permissions?.query({ name: "clipboard-read" as PermissionName })
        .then(state => {
            allowed = state.state === "granted";
            state.onchange = () => { allowed = state.state === "granted"; };
        })
        .catch(() => undefined); // a browser that does not tell: never read ahead

    const write = (text: string) => {
        pending = undefined;
        board()?.writeText(text).catch(() => { pending = text; });
    };

    /** The text of this device goes to the worker: it knows whether it is new. */
    const sync = () => {
        if (!active()) return;
        if (pending !== undefined) return write(pending);
        const b = board();
        if (!allowed || reading || !b?.readText) return;
        reading = true;
        b.readText().then(text => { if (text !== "" && !stopped) post({ t: W.CLIPBOARD, text }); })
            .catch(() => undefined) // not allowed, or the page has no focus
            .finally(() => { reading = false; });
    };

    /** A click on the canvas makes the desk the one in use: the input of the desk took the focus before this listener. */
    const onPointerDown = sync;
    let focused = canvas.ownerDocument.defaultView ?? window;
    focused.addEventListener("focus", sync);
    canvas.addEventListener("pointerdown", onPointerDown);
    sync();

    return {
        set(text) {
            if (active()) write(text);
            else pending = text;
        },
        sync,
        moved() {
            focused.removeEventListener("focus", sync);
            focused = canvas.ownerDocument.defaultView ?? window;
            focused.addEventListener("focus", sync);
        },
        stop() {
            stopped = true;
            focused.removeEventListener("focus", sync);
            canvas.removeEventListener("pointerdown", onPointerDown);
        },
    };
}
