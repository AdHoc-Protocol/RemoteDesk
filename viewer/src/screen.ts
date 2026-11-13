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
    /** Tells the worker again whether the screen is seen: the page, and the desk in it ( `seen` ). */
    visibility(): void;
    /** What the worker tells of the video. */
    take(message: Extract<ToMain, { t: M.SIZE | M.SCREEN | M.DISPLAYS | M.CURSOR | M.HOST_AT | M.STATS }>): void;
    stop(): void;
}

/**
 * Shows `canvas` in `host`: `select` is filled with the displays of the machine, `stats` shows how the frames go; `layer` - the element
 * the desk is in ( full screen shows only it ): the pointer of the machine is drawn there; `seen` - the desk is shown on the page ( one of
 * several may be hidden ).
 */
export function start(canvas: HTMLCanvasElement, host: HTMLElement, layer: HTMLElement, select: HTMLSelectElement, stats: HTMLElement, words: Words,
    seen: () => boolean, post: Post): Screen {
    host.replaceChildren(canvas);

    const marker = document.createElement("img"); // the pointer of the machine, when the one of the page is not over the canvas
    marker.style.cssText = "position:fixed;pointer-events:none;display:none;z-index:2";
    layer.appendChild(marker);

    let frameWidth = 300; // of a canvas before its first frame
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
        const ratio = window.devicePixelRatio || 1;
        post({ t: W.FIT, on: true, now, w: Math.min(65535, Math.round(r.width * ratio)), h: Math.min(65535, Math.round(r.height * ratio)) });
    };
    // the page or the frame changed size: the pointer of the machine moves with the canvas, not with each frame drawn
    const resized = new ResizeObserver(() => { if (fitting) fit(false); placeMarker(); });
    resized.observe(host);
    resized.observe(canvas);

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
    const onVisibility = () => post({ t: W.VISIBLE, on: !document.hidden && seen() });
    document.addEventListener("visibilitychange", onVisibility);
    if (document.hidden || !seen()) onVisibility();

    return {
        canvas,
        frameWidth: () => frameWidth,
        fit(on: boolean) {
            fitting = on;
            if (on) fit(true);
            else post({ t: W.FIT, on: false, now: true, w: 0, h: 0 });
        },
        visibility: onVisibility,
        take(m) {
            switch (m.t) {
                case M.SIZE:
                    frameWidth = m.w;
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
        stop() {
            resized.disconnect();
            canvas.removeEventListener("pointerenter", onEnter);
            canvas.removeEventListener("pointerleave", onLeave);
            select.removeEventListener("change", onSelect);
            document.removeEventListener("visibilitychange", onVisibility);
            marker.remove();
            host.replaceChildren();
            stats.textContent = "";
        },
    };
}
