// The clipboard of the page and the machine: text the machine copied is set on the clipboard of this device, text copied here goes
// to the worker of the session ( ./worker/clipboard.ts, which sends it when it is new ) when the page is about to be used - it gets
// the focus, the canvas is clicked, Ctrl ( ⌘ ) or Shift goes down before a paste. The browser lets a page read the clipboard only
// then, and asks the user once. Of several desks on a page only the one in use ( `active` ) does it: text another machine copied waits
// until its desk is used.

import { W, type ToWorker } from "./messages";

type Post = (message: ToWorker) => void;

export interface Clipboard {
    /** Text the machine copied. */
    set(text: string): void;
    /** The desk is the one in use now: what waits goes. */
    sync(): void;
    stop(): void;
}

/** `active` - the desk of this clipboard is the one in use: it has the focus of the page. */
export function start(canvas: HTMLCanvasElement, active: () => boolean, post: Post): Clipboard {
    /** Text of the machine the browser did not let the page set yet: set when the page gets the focus. */
    let pending: string | undefined;
    let reading = false;
    let stopped = false; // a read that ends after the stop is not sent

    const write = (text: string) => {
        pending = undefined;
        navigator.clipboard?.writeText(text).catch(() => { pending = text; });
    };

    /** The text of this device goes to the worker: it knows whether it is new. */
    const sync = () => {
        if (!active()) return;
        if (pending !== undefined) return write(pending);
        if (reading || !navigator.clipboard?.readText) return;
        reading = true;
        navigator.clipboard.readText().then(text => { if (text !== "" && !stopped) post({ t: W.CLIPBOARD, text }); })
            .catch(() => undefined) // not allowed, or the page has no focus
            .finally(() => { reading = false; });
    };

    const onKeyDown = (e: KeyboardEvent) => {
        if (e.repeat) return;
        if (!active()) return;
        if (e.key === "Control" || e.key === "Meta" || e.key === "Shift") sync();
    };

    /** A click on the canvas makes the desk the one in use: the input of the desk took the focus before this listener. */
    const onPointerDown = sync;
    window.addEventListener("focus", sync);
    canvas.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown, true);
    sync();

    return {
        set(text) {
            if (active()) write(text);
            else pending = text;
        },
        sync,
        stop() {
            stopped = true;
            window.removeEventListener("focus", sync);
            canvas.removeEventListener("pointerdown", onPointerDown);
            document.removeEventListener("keydown", onKeyDown, true);
        },
    };
}
