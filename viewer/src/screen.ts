// The screen of the session on the page: the canvas the worker draws the video on ( its control given away, ./worker/video.ts ),
// the pointer of the machine over it, the displays of the machine to choose from, the line of how the frames go. The size of the
// screen of the page for the resolution fitted to it, and whether the page is seen, go to the worker.

import { M, W, type Display, type ToMain, type ToWorker } from "./messages";

type Post = (message: ToWorker, transfer?: Transferable[]) => void;

/** What the screen says: the name of a display in the choice, the line of how the frames go. */
export interface Words {
    display(d: Display, inUse: boolean): string;
    stats(m: Extract<ToMain, { t: M.STATS }>): string;
}

/** A canvas for the next connection, and its control for the worker: a canvas gives its control away once. */
export function canvasFor(): { canvas: HTMLCanvasElement; offscreen: OffscreenCanvas } {
    const canvas = document.createElement("canvas");
    canvas.tabIndex = 0;
    return { canvas, offscreen: canvas.transferControlToOffscreen() };
}

export interface Screen {
    /** The canvas the desktop is drawn on: the input of the page is taken from it. */
    readonly canvas: HTMLCanvasElement;
    /** The width of the frames, pixels of the machine: the canvas has it, the element does not tell. */
    frameWidth(): number;
    /** The resolution of the display shown follows the size of the screen of the page, or is restored. */
    fit(on: boolean): void;
    /** 1:1 - a pixel of the frames is a pixel of the device, the view moved by the pointer or a finger at an edge; else the whole frame fits the screen. */
    oneToOne(on: boolean): void;
    /** Two fingers zoom the view ( ./input.ts tells ): `ratio` - their distance to the one they began at, (`x`, `y`) - the point between them. */
    pinch(phase: "start" | "move" | "end", ratio: number, x: number, y: number): void;
    /** Tells the worker again whether the screen is seen: the page, and the desk in it ( `seen` ). */
    visibility(): void;
    /** What the worker tells of the video. */
    take(message: Extract<ToMain, { t: M.SIZE | M.SCREEN | M.DISPLAYS | M.CURSOR | M.HOST_AT | M.STATS }>): void;
    /** The desk went into another window: what is watched is watched there. */
    moved(): void;
    stop(): void;
}

/** How large the view is: the whole screen, 1:1, or a zoom a pinch made - `percent` of 1:1. */
export type View = { kind: "whole" } | { kind: "one" } | { kind: "zoom"; percent: number };

/**
 * Shows `canvas` in `host`: `select` is filled with the displays of the machine, `stats` shows how the frames go; `layer` - the element
 * the desk is in ( full screen shows only it ): the pointer of the machine is drawn there; `seen` - the desk is shown on the page ( one of
 * several may be hidden ); `viewed` - how large the view is now, told when it changes.
 */
export function start(canvas: HTMLCanvasElement, host: HTMLElement, layer: HTMLElement, select: HTMLSelectElement, stats: HTMLElement, words: Words,
    seen: () => boolean, post: Post, viewed: (view: View) => void): Screen {
    host.replaceChildren(canvas);
    /** The window the desk is in: the page's, or one of its own ( src/app.ts ). */
    const view = () => host.ownerDocument.defaultView ?? window;

    const marker = document.createElement("img"); // the pointer of the machine, when the one of the page is not over the canvas
    marker.style.cssText = "position:fixed;pointer-events:none;display:none;z-index:2";
    layer.appendChild(marker);

    let frameWidth = 300, frameHeight = 150; // of a canvas before its first frame
    let screen = 0;
    let pointerOver = false;
    let hotspotX = 0, hotspotY = 0;
    const sprite = document.createElement("canvas"); // the shapes of the pointer are drawn on it, one after another
    const spriteContext = sprite.getContext("2d")!;
    let hostX = 0, hostY = 0, hostScreen = -1; // -1: the machine did not tell yet
    let displays: Display[] = [];
    let active = -1;
    let fitting = false;

    /** The displays to choose from; the one the user of the machine works on is marked. */
    const options = () => {
        select.replaceChildren(...displays.map(d => {
            const o = document.createElement("option");
            o.value = String(d.screen);
            o.textContent = words.display(d, d.screen === active && displays.length > 1);
            return o;
        }));
        select.hidden = displays.length < 2;
        select.value = String(screen);
    };

    // ── the resolution fitted to the page ──

    /** The screen of the page, pixels of the device: the worker waits for the window to settle unless `now`. */
    const fit = (now: boolean) => {
        const r = host.getBoundingClientRect();
        const ratio = view().devicePixelRatio || 1;
        post({ t: W.FIT, on: true, now, w: Math.min(65535, Math.round(r.width * ratio)), h: Math.min(65535, Math.round(r.height * ratio)) });
    };
    // the page or the frame changed size: the pointer of the machine moves with the canvas, not with each frame drawn
    // made by the window the desk is in: that window measures it
    const sized = () => { if (fitting) fit(false); placeMarker(); };
    let resized = new (view().ResizeObserver)(sized);
    resized.observe(host);
    resized.observe(canvas);

    // ── the size of the view: the whole screen fitted; 1:1 - a pixel of the frames on a pixel of the device; or what a pinch made ──
    //
    // As the viewers of phones do it ( bVNC "one to one", Microsoft Remote Desktop, the pinch of AnyDesk ): the view follows the
    // pointer or a finger near an edge, two fingers apart or together zoom it about the point between them. The canvas is moved, not
    // scrolled: the input reads where it is ( getBoundingClientRect ), so a point of the page is still the point of the machine.

    /** Pixels of the page a pixel of the frames is: undefined - the whole frame fits the screen. */
    let zoom: number | undefined;
    let panX = 0, panY = 0; // where the view begins on the canvas, pixels of the page
    let edgeX = 0, edgeY = 0; // the speed of the pan at an edge: -1 … 1
    let panning = 0; // the frame of the pan asked: 0 - none
    let panWindow: Window = window; // the window it was asked of
    /** A pinch going: the zoom it began at, the point of the frames under the fingers then. */
    let pinchFrom: { zoom: number; fx: number; fy: number } | undefined;
    /** The most a pinch zooms: pixels of the page a pixel of the frames. */
    const MAX_ZOOM = 4;

    /** The zoom of the whole frame fitted, as the style fits it: never larger than the frame. */
    const fitted = () => {
        const r = host.getBoundingClientRect();
        return Math.min(1, r.width / Math.max(1, frameWidth), r.height / Math.max(1, frameHeight));
    };
    const oneToOne = () => 1 / (view().devicePixelRatio || 1);

    /** Tells how large the view is. */
    const tell = () => viewed(zoom === undefined ? { kind: "whole" } : Math.abs(zoom - oneToOne()) < 1e-3 ? { kind: "one" }
        : { kind: "zoom", percent: Math.round(zoom / oneToOne() * 100) });

    /** The canvas as large as the zoom makes it, moved to the view; or as before, fitted. */
    function layout(): void {
        host.classList.toggle("rd-one", zoom !== undefined);
        if (zoom === undefined) {
            canvas.style.width = canvas.style.height = canvas.style.transform = "";
            return placeMarker();
        }
        const w = frameWidth * zoom, h = frameHeight * zoom;
        const r = host.getBoundingClientRect();
        canvas.style.width = `${w}px`;
        canvas.style.height = `${h}px`;
        // larger than the view: within it; smaller: in its middle
        panX = w > r.width ? Math.min(Math.max(panX, 0), w - r.width) : (w - r.width) / 2;
        panY = h > r.height ? Math.min(Math.max(panY, 0), h - r.height) : (h - r.height) / 2;
        canvas.style.transform = `translate(${-panX}px, ${-panY}px)`;
        placeMarker();
    }

    /** The pointer or a finger at an edge of the view: how fast the view goes there, by how deep in the edge it is. */
    const onPan = (e: PointerEvent) => {
        if (zoom === undefined || pinchFrom || host.ownerDocument.pointerLockElement === canvas) return;
        const r = host.getBoundingClientRect(), edge = Math.max(24, Math.min(72, r.width / 10, r.height / 10));
        const x = e.clientX - r.left, y = e.clientY - r.top;
        const depth = (at: number, size: number) => at < edge ? -(edge - Math.max(at, 0)) / edge : at > size - edge ? (Math.min(at, size) - (size - edge)) / edge : 0;
        edgeX = depth(x, r.width);
        edgeY = depth(y, r.height);
        if ((edgeX || edgeY) && !panning) panning = (panWindow = view()).requestAnimationFrame(pan);
    };
    const panStops = () => { edgeX = edgeY = 0; };

    /** One step of the pan: up to 28 pixels a frame, deep in the edge. */
    function pan(): void {
        panning = 0;
        if (zoom === undefined || pinchFrom || (!edgeX && !edgeY)) return;
        panX += edgeX * 28;
        panY += edgeY * 28;
        layout();
        panning = (panWindow = view()).requestAnimationFrame(pan);
    }

    host.addEventListener("pointermove", onPan);
    host.addEventListener("pointerdown", onPan);
    host.addEventListener("pointerleave", panStops);
    host.addEventListener("pointerup", panStops);
    host.addEventListener("pointercancel", panStops);
    const relayout = () => { if (zoom !== undefined) layout(); };
    let relaid = new (view().ResizeObserver)(relayout);
    relaid.observe(host);

    // ── the pointer of the machine ──

    /** The pointer of the machine is drawn by the page only when the page's own pointer is not over the canvas. */
    function placeMarker(): void {
        if (pointerOver || hostScreen !== screen || !marker.src) {
            marker.style.display = "none";
            return;
        }
        const r = canvas.getBoundingClientRect();
        marker.style.left = `${r.left + hostX / 65535 * r.width - hotspotX}px`;
        marker.style.top = `${r.top + hostY / 65535 * r.height - hotspotY}px`;
        marker.style.display = "";
    }

    const onEnter = () => { pointerOver = true; placeMarker(); };
    const onLeave = () => { pointerOver = false; placeMarker(); };
    canvas.addEventListener("pointerenter", onEnter);
    canvas.addEventListener("pointerleave", onLeave);

    const onSelect = () => post({ t: W.SELECT, screen: Number(select.value) });
    select.addEventListener("change", onSelect);

    /**
     * A screen not seen - the page in another tab, a minimized window, the desk hidden on the page - takes no video: the machine neither
     * captures nor encodes for it.
     */
    const onVisibility = () => post({ t: W.VISIBLE, on: !host.ownerDocument.hidden && seen() });
    let watched = host.ownerDocument;
    watched.addEventListener("visibilitychange", onVisibility);
    if (watched.hidden || !seen()) onVisibility();

    return {
        canvas,
        frameWidth: () => frameWidth,
        fit(on: boolean) {
            fitting = on;
            if (on) fit(true);
            else post({ t: W.FIT, on: false, now: true, w: 0, h: 0 });
        },
        oneToOne(on: boolean) {
            zoom = on ? oneToOne() : undefined;
            panStops();
            layout();
            tell();
        },
        pinch(phase, ratio, x, y) {
            if (phase === "start") {
                const from = zoom ?? fitted(), r = canvas.getBoundingClientRect();
                pinchFrom = { zoom: from, fx: (x - r.left) / from, fy: (y - r.top) / from };
                panStops();
                return;
            }
            if (!pinchFrom) return;
            if (phase === "end") {
                pinchFrom = undefined;
                return tell();
            }
            // the point of the frames that was under the fingers stays under them, wherever they go
            const least = fitted(), z = Math.min(MAX_ZOOM, Math.max(least, pinchFrom.zoom * ratio));
            if (z <= least * 1.001) zoom = undefined;
            else {
                const r = host.getBoundingClientRect();
                zoom = z;
                panX = pinchFrom.fx * z - (x - r.left);
                panY = pinchFrom.fy * z - (y - r.top);
            }
            layout();
            tell();
        },
        visibility: onVisibility,
        take(m) {
            switch (m.t) {
                case M.SIZE:
                    frameWidth = m.w;
                    frameHeight = m.h;
                    if (zoom !== undefined) layout();
                    return;
                case M.SCREEN:
                    screen = m.screen;
                    select.value = String(screen);
                    return placeMarker();
                case M.DISPLAYS:
                    displays = m.displays;
                    active = m.active;
                    return options();
                case M.CURSOR: {
                    sprite.width = m.w;
                    sprite.height = m.h;
                    spriteContext.putImageData(new ImageData(new Uint8ClampedArray(m.rgba), m.w, m.h), 0, 0);
                    const url = sprite.toDataURL();
                    hotspotX = m.x;
                    hotspotY = m.y;
                    canvas.style.cursor = `url(${url}) ${m.x} ${m.y}, default`;
                    marker.src = url;
                    return placeMarker();
                }
                case M.HOST_AT:
                    hostX = m.x;
                    hostY = m.y;
                    hostScreen = m.screen;
                    return placeMarker();
                case M.STATS:
                    stats.textContent = words.stats(m);
                    return;
            }
        },
        moved() {
            watched.removeEventListener("visibilitychange", onVisibility);
            watched = host.ownerDocument;
            watched.addEventListener("visibilitychange", onVisibility);
            resized.disconnect();
            relaid.disconnect();
            resized = new (view().ResizeObserver)(sized);
            resized.observe(host);
            resized.observe(canvas);
            relaid = new (view().ResizeObserver)(relayout);
            relaid.observe(host);
            onVisibility();
        },
        stop() {
            resized.disconnect();
            relaid.disconnect();
            if (panning) panWindow.cancelAnimationFrame(panning);
            host.removeEventListener("pointermove", onPan);
            host.removeEventListener("pointerdown", onPan);
            host.removeEventListener("pointerleave", panStops);
            host.removeEventListener("pointerup", panStops);
            host.removeEventListener("pointercancel", panStops);
            host.classList.remove("rd-one");
            canvas.removeEventListener("pointerenter", onEnter);
            canvas.removeEventListener("pointerleave", onLeave);
            select.removeEventListener("change", onSelect);
            watched.removeEventListener("visibilitychange", onVisibility);
            marker.remove();
            host.replaceChildren();
            stats.textContent = "";
        },
    };
}
