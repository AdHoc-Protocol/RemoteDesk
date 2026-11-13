// Marks over the screen of the machine: while ✏️ is on, the mouse or a finger draws instead of controlling - a line drawn, a ring
// where it tapped, an arrow when ➚ is on. The machine shows each over its screen for its user, holds it a moment and fades it
// ( ../host/src/marks.rs ); this page sees it in the video, as the user does. Every viewer draws, not only the one that controls.
//
// A press is a tap until the pointer moves away or holds: then it is a stroke, and its points go as they come, once an animation
// frame - so the user sees the line grow while it is drawn.

import { W, type ToWorker } from "./messages";

type Post = (message: ToWorker) => void;

export interface Marking {
    /** Drawing on, or off: off, the input goes to the machine again. */
    on(on: boolean): void;
    stop(): void;
}

/** How far, CSS pixels, and how long a press may go and still be a tap. */
const TAP_MOVE = 6;
const TAP_MS = 350;
/** A pack carries this many points at most: x and y each. */
const PACK = 256;
/** `Mark.Kind`. */
const LINE = 0, RING = 1, ARROW = 2;

/**
 * Draws over `host` - the element of the screen -; `canvas` gives the canvas of the video the points are fractions of; `color` and
 * `arrow` - what the buttons of the desk chose.
 */
export function start(host: HTMLElement, canvas: () => HTMLCanvasElement | undefined, post: Post, color: () => number, arrow: () => boolean): Marking {
    const pad = host.ownerDocument.createElement("div");
    pad.className = "rd-draw";
    host.appendChild(pad);

    let stroke = 0;
    /** The press drawing now: its pointer, where and when it began. */
    let press: { id: number; x: number; y: number; at: number; kind: number; going: boolean } | undefined;
    let lastX = 0, lastY = 0;
    /** Points not sent yet: x, y, … */
    const pending: number[] = [];
    let frame = 0;

    const view = () => host.ownerDocument.defaultView ?? window;

    /** The point of the page as fractions of the canvas, 0 … 65535. */
    const fraction = (x: number, y: number): [number, number] | undefined => {
        const c = canvas();
        if (!c) return undefined;
        const r = c.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return undefined;
        const f = (v: number) => Math.round(Math.min(1, Math.max(0, v)) * 65535);
        return [f((x - r.left) / r.width), f((y - r.top) / r.height)];
    };

    const send = (kind: number, done: boolean) => {
        do {
            const points = pending.splice(0, PACK * 2);
            post({ t: W.MARK, kind, stroke, done: done && pending.length === 0, color: color(), points });
        } while (pending.length > 0);
    };

    const flush = () => {
        frame = 0;
        if (press?.going && pending.length > 0) send(press.kind, false);
    };

    const add = (x: number, y: number) => {
        const f = fraction(x, y);
        if (!f) return;
        pending.push(f[0], f[1]);
        lastX = x;
        lastY = y;
    };

    const down = (e: PointerEvent) => {
        if (press || (e.pointerType === "mouse" && e.button !== 0)) return;
        e.preventDefault();
        pad.setPointerCapture(e.pointerId);
        stroke = (stroke + 1) & 0xff;
        press = { id: e.pointerId, x: e.clientX, y: e.clientY, at: performance.now(), kind: arrow() ? ARROW : LINE, going: false };
        pending.length = 0;
        add(e.clientX, e.clientY);
    };

    const move = (e: PointerEvent) => {
        if (!press || e.pointerId !== press.id) return;
        if (!press.going) {
            if (Math.hypot(e.clientX - press.x, e.clientY - press.y) < TAP_MOVE && performance.now() - press.at < TAP_MS) return;
            press.going = true; // a stroke, not a tap
        }
        if (Math.hypot(e.clientX - lastX, e.clientY - lastY) < 2) return;
        add(e.clientX, e.clientY);
        if (!frame) frame = view().requestAnimationFrame(flush);
    };

    const up = (e: PointerEvent) => {
        if (!press || e.pointerId !== press.id) return;
        const p = press;
        press = undefined;
        if (frame) {
            view().cancelAnimationFrame(frame);
            frame = 0;
        }
        if (!p.going && e.type === "pointerup") {
            // a tap: a ring there - "here"
            if (pending.length < 2) return;
            pending.length = 2;
            return send(RING, true);
        }
        if (!p.going) return void (pending.length = 0); // canceled before it was a stroke: nothing was drawn
        add(e.clientX, e.clientY);
        send(p.kind, true);
    };

    pad.addEventListener("pointerdown", down);
    pad.addEventListener("pointermove", move);
    pad.addEventListener("pointerup", up);
    pad.addEventListener("pointercancel", up);
    const noMenu = (e: Event) => e.preventDefault();
    pad.addEventListener("contextmenu", noMenu);

    return {
        on(on) {
            pad.classList.toggle("rd-drawing", on);
            if (!on && press) {
                const p = press;
                press = undefined;
                if (p.going) send(p.kind, true);
            }
        },
        stop() {
            if (frame) view().cancelAnimationFrame(frame);
            pad.remove();
        },
    };
}
