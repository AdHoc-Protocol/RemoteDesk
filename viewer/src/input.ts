// The input of the page, posted to the worker of the session ( ./worker/input.ts makes the packs ): keys by their physical place
// ( USB HID usage - the layout is the machine's ), text of input methods, the mouse in fractions of the canvas, the wheel in ticks,
// the pointer locked in the page, touch. The pointer and the wheel go once an animation frame of the page; a key, text, a button
// or a touch goes at once, after what the pointer did before it.

import { W, Touch, type ToWorker } from "./messages";
import { usbOf } from "./keymap";

type Post = (message: ToWorker) => void;

const mac = /Mac|iPhone|iPad/.test(navigator.platform);
const clamp = (v: number) => Math.max(-32768, Math.min(32767, Math.round(v)));
/** `v` of `size` as 0 … 65535. */
const fraction = (v: number, size: number) => Math.round(Math.min(1, Math.max(0, v / size)) * 65535);

export interface Input {
    lockPointer(): void;
    /** The keys of the page come to this desk now. */
    focus(): void;
    stop(): void;
}

/**
 * `frameWidth` - the width of the frames, pixels of the machine: what a pixel of the page is on the machine; `layer` - the element the desk
 * is in ( full screen shows only it ): the field the keys come to is there.
 */
export function start(canvas: HTMLCanvasElement, layer: HTMLElement, frameWidth: () => number, post: Post): Input {
    // the keys come to an invisible field over the canvas: input methods and soft keyboards type into a field
    const field = document.createElement("textarea");
    field.setAttribute("autocapitalize", "off");
    field.setAttribute("autocomplete", "off");
    field.spellcheck = false;
    field.style.cssText = "position:fixed;left:0;top:0;width:1px;height:1px;opacity:0;border:0;padding:0;resize:none;z-index:-1";
    layer.appendChild(field);
    canvas.style.touchAction = "none";

    const held = new Set<number>();
    let lastEventTime = 0;

    /** Before a burst of input: the machine returns it with the first frame after it. */
    const stamp = () => {
        const now = performance.now();
        if (now - lastEventTime < 50) return;
        lastEventTime = now;
        post({ t: W.STAMP, ms: now });
    };

    // ── keys ──

    const key = (usb: number, pressed: boolean, e?: KeyboardEvent) => {
        if (frameAsked) flush(); // the pointer and the wheel first, as they came
        if (e && pressed) {
            if (mac) post({ t: W.KEY, usb, pressed, caps: e.getModifierState("CapsLock") });
            else post({ t: W.KEY, usb, pressed, caps: e.getModifierState("CapsLock"), num: e.getModifierState("NumLock") });
        } else post({ t: W.KEY, usb, pressed });
        pressed ? held.add(usb) : held.delete(usb);
    };

    const text = (data: string) => {
        if (!data) return;
        stamp();
        if (frameAsked) flush();
        post({ t: W.TEXT, text: data });
    };

    // ── a paste: the text of this device goes first, then the keys - the machine pastes it ──
    //
    // Ctrl ( ⌘ ) + V and Shift + Insert are left to the browser: it pastes into the field, and its paste brings the text with no
    // asking - reading the clipboard would ask the user. The text goes to the machine, then the key, held back till then. No paste
    // comes ( no text on the clipboard ): the key goes in a moment anyway.

    /** The key of a paste held back: the paste goes first. */
    let pasteKey: { usb: number; e: KeyboardEvent; timer: ReturnType<typeof setTimeout> } | undefined;

    const isPaste = (e: KeyboardEvent) =>
        !e.repeat && !e.altKey && ((e.code === "KeyV" && (e.ctrlKey || e.metaKey)) || (e.code === "Insert" && e.shiftKey && !e.ctrlKey));

    /** The key of the paste goes: after its text, or after waiting for one. */
    const pasteGoes = () => {
        if (!pasteKey) return;
        clearTimeout(pasteKey.timer);
        const { usb, e } = pasteKey;
        pasteKey = undefined;
        key(usb, true, e);
    };

    const onPaste = (e: ClipboardEvent) => {
        e.preventDefault(); // into the field goes nothing: the text goes to the machine
        const data = e.clipboardData?.getData("text/plain") ?? "";
        if (data !== "") post({ t: W.CLIPBOARD, text: data.replace(/\r\n?/g, "\n") });
        pasteGoes();
    };

    const onKeyDown = (e: KeyboardEvent) => {
        if (e.isComposing || e.keyCode === 229) return; // an input method composes: its text comes with compositionend
        const usb = usbOf[e.code];
        if (usb !== undefined && isPaste(e)) {
            stamp();
            if (pasteKey) pasteGoes();
            pasteKey = { usb, e, timer: setTimeout(pasteGoes, 150) };
            return; // not prevented: the browser pastes
        }
        e.preventDefault();
        stamp();
        if (usb !== undefined) key(usb, true, e);
        else if (e.key.length === 1) text(e.key);
    };

    const onKeyUp = (e: KeyboardEvent) => {
        if (e.isComposing) return;
        const usb = usbOf[e.code];
        if (usb === undefined) return;
        if (pasteKey?.usb === usb) pasteGoes(); // let go before the paste came: down first, then up
        e.preventDefault();
        if (held.has(usb)) key(usb, false);
    };

    const onComposed = (e: CompositionEvent) => {
        text(e.data);
        field.value = "";
    };

    /** A soft keyboard types without keys the page can name: what it typed is text. */
    const onInput = (e: Event) => {
        const ie = e as InputEvent;
        if (!ie.isComposing && ie.inputType === "insertText" && ie.data) text(ie.data);
        if (!ie.isComposing) field.value = "";
    };

    /** The page lost the focus: what it held goes up on the machine. */
    const releaseAll = () => {
        for (const usb of held) key(usb, false); // deleting from a Set while going over it is safe
    };

    field.addEventListener("keydown", onKeyDown);
    field.addEventListener("paste", onPaste);
    field.addEventListener("keyup", onKeyUp);
    field.addEventListener("compositionend", onComposed);
    field.addEventListener("input", onInput);
    field.addEventListener("blur", releaseAll);
    window.addEventListener("blur", releaseAll);

    // ── the mouse and the pen ──

    // what the pointer and the wheel did since the last frame of the page: posted once a frame, before any button or key; the place
    // on the canvas is worked out then, once
    let moved = false, pointerX = 0, pointerY = 0;
    let deltaX = 0, deltaY = 0; // pixels of the page
    let wheelX = 0, wheelY = 0; // 120ths of a notch: what is less than one waits for more
    let frameAsked = false;
    let stopped = false; // a frame asked for before the stop comes after it
    const flush = () => {
        frameAsked = false;
        const wx = Math.trunc(wheelX), wy = Math.trunc(wheelY);
        if (stopped || !moved && !deltaX && !deltaY && !wx && !wy) return;
        let x = 0, y = 0, dx = 0, dy = 0;
        if (moved || deltaX || deltaY) {
            const r = canvas.getBoundingClientRect();
            if (moved) {
                x = fraction(pointerX - r.left, r.width);
                y = fraction(pointerY - r.top, r.height);
            }
            const scale = frameWidth() / Math.max(1, r.width);
            dx = clamp(deltaX * scale);
            dy = clamp(deltaY * scale);
        }
        post({ t: W.POINTER, moved, x, y, dx, dy, wx: clamp(wx), wy: clamp(wy) });
        moved = false;
        deltaX = deltaY = 0;
        wheelX -= wx;
        wheelY -= wy;
    };
    const later = () => {
        if (!frameAsked) {
            frameAsked = true;
            requestAnimationFrame(flush);
        }
    };

    const locked = () => document.pointerLockElement === canvas;

    const button = (e: PointerEvent, down: boolean) => {
        if (e.button < 0 || e.button > 4) return;
        flush();
        stamp();
        const r = canvas.getBoundingClientRect();
        post({ t: W.BUTTON, button: e.button, down, x: fraction(e.clientX - r.left, r.width), y: fraction(e.clientY - r.top, r.height), locked: locked() });
    };

    // ── touch ──

    const touches = new Map<number, number>(); // pointerId → the id of the point on the wire
    let used = 0; // the ids on the wire taken, a bit each
    const touchMoved = new Map<number, PointerEvent>();
    let touchFrame = false;

    /** Adds the point of `e` to `points`: `r` - the canvas on the page, `scale` - pixels of the machine a pixel of the page. */
    const point = (points: number[], e: PointerEvent, id: number, r: DOMRect, scale: number) => {
        points.push(
            id,
            fraction(e.clientX - r.left, r.width), fraction(e.clientY - r.top, r.height),
            Math.min(4095, Math.round(e.width / 2 * scale)), Math.min(4095, Math.round(e.height / 2 * scale)),
            Math.round(e.twist ?? 0) % 360,
            Math.round(e.pressure * 255),
        );
    };
    const one = (e: PointerEvent, id: number) => {
        const r = canvas.getBoundingClientRect(), points: number[] = [];
        point(points, e, id, r, frameWidth() / Math.max(1, r.width));
        return points;
    };

    const touchEvent = (kind: Touch, points: number[]) => {
        if (frameAsked) flush();
        post({ t: W.TOUCH, kind, points });
    };

    const flushTouch = () => {
        touchFrame = false;
        if (stopped || touchMoved.size === 0) return;
        const r = canvas.getBoundingClientRect(), scale = frameWidth() / Math.max(1, r.width);
        const points: number[] = [];
        for (const e of touchMoved.values()) {
            const id = touches.get(e.pointerId);
            if (id !== undefined) point(points, e, id, r, scale);
        }
        touchMoved.clear();
        if (points.length) touchEvent(Touch.MOVE, points);
    };

    // ── the events of the canvas ──

    const onPointerDown = (e: PointerEvent) => {
        e.preventDefault();
        field.focus({ preventScroll: true });
        canvas.setPointerCapture(e.pointerId);
        if (e.pointerType === "touch") {
            const free = ~used & (used + 1); // the lowest free one, its bit
            const id = free === 0 ? 31 : 31 - Math.clz32(free);
            used |= 1 << id;
            touches.set(e.pointerId, id);
            stamp();
            touchEvent(Touch.START, one(e, id));
        } else button(e, true);
    };

    const onPointerUp = (e: PointerEvent) => {
        if (e.pointerType === "touch") {
            const id = touches.get(e.pointerId);
            if (id === undefined) return;
            flushTouch();
            touches.delete(e.pointerId);
            used &= ~(1 << id);
            touchEvent(e.type === "pointercancel" ? Touch.CANCEL : Touch.END, one(e, id));
        } else if (e.type === "pointerup") button(e, false);
    };

    const onPointerMove = (e: PointerEvent) => {
        if (e.pointerType === "touch") {
            if (!touches.has(e.pointerId)) return;
            touchMoved.set(e.pointerId, e);
            if (!touchFrame) {
                touchFrame = true;
                requestAnimationFrame(flushTouch);
            }
            return;
        }
        if (locked()) {
            deltaX += e.movementX;
            deltaY += e.movementY;
        } else {
            pointerX = e.clientX;
            pointerY = e.clientY;
            moved = true;
        }
        later();
    };

    /** 120ths of a notch: positive - away from the user, up or right; a notch is 100 pixels in Chrome on Windows, 3 lines elsewhere. */
    const onWheel = (e: WheelEvent) => {
        e.preventDefault();
        stamp();
        const per = e.deltaMode === WheelEvent.DOM_DELTA_PIXEL ? 120 / 100 : e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 120 / 3 : 120;
        wheelX -= e.deltaX * per;
        wheelY -= e.deltaY * per;
        later();
    };

    const noMenu = (e: Event) => e.preventDefault();

    canvas.addEventListener("pointerdown", onPointerDown);
    canvas.addEventListener("pointerup", onPointerUp);
    canvas.addEventListener("pointercancel", onPointerUp);
    canvas.addEventListener("pointermove", onPointerMove);
    canvas.addEventListener("wheel", onWheel, { passive: false });
    canvas.addEventListener("contextmenu", noMenu);
    canvas.addEventListener("auxclick", noMenu);
    field.focus({ preventScroll: true });

    return {
        focus() {
            field.focus({ preventScroll: true });
        },
        lockPointer() {
            field.focus({ preventScroll: true });
            void canvas.requestPointerLock?.();
        },
        stop() {
            releaseAll();
            stopped = true;
            if (locked()) document.exitPointerLock();
            field.removeEventListener("keydown", onKeyDown);
            field.removeEventListener("paste", onPaste);
            if (pasteKey) clearTimeout(pasteKey.timer);
            field.removeEventListener("keyup", onKeyUp);
            field.removeEventListener("compositionend", onComposed);
            field.removeEventListener("input", onInput);
            field.removeEventListener("blur", releaseAll);
            window.removeEventListener("blur", releaseAll);
            canvas.removeEventListener("pointerdown", onPointerDown);
            canvas.removeEventListener("pointerup", onPointerUp);
            canvas.removeEventListener("pointercancel", onPointerUp);
            canvas.removeEventListener("pointermove", onPointerMove);
            canvas.removeEventListener("wheel", onWheel);
            canvas.removeEventListener("contextmenu", noMenu);
            canvas.removeEventListener("auxclick", noMenu);
            field.remove();
            touches.clear();
            touchMoved.clear();
        },
    };
}
