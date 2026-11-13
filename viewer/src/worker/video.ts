// The video of the session in the worker: H.264 decoded by WebCodecs, drawn on the OffscreenCanvas the page gave away; every frame
// shown is acknowledged, so the machine sends the next ones. The displays of the machine, the shape and the place of its pointer,
// the size of the frames and how they go are told to the page, which draws the rest ( ../screen.ts ).

import Viewer from "../../protocol/gen/Viewer";
import Session from "../../protocol/gen/Session/Connection";
import Actor0 from "../../protocol/gen/Session/Actor0";
import Base_ from "../../protocol/gen/Base_";
import { M, type Display } from "../messages";
import { toPage } from "./page";

const Working = Actor0.State.Working;

export interface Video {
    /** The display shown now: the number of the machine. */
    screen(): number;
    /** Where the pointer of the machine is, as it reported it last. */
    hostAt(): Viewer.FractionalCoordinate | undefined;
    /** The resolution of the display shown follows `w` × `h` - the screen of the page, pixels of the device - or is restored;
     *  `now` - else once the window settles. */
    fit(on: boolean, now: boolean, w: number, h: number): void;
    /** The most frames a second the machine sends. */
    framerate(fps: number): void;
    /** The page is seen, or not: a page not seen - another tab, a minimized window - takes no video, the machine neither captures
     *  nor encodes for it. */
    visible(on: boolean): void;
    /** Shows the display `n` of the machine. */
    select(n: number): void;
    /** This viewer controls the machine, or not: the fit and the time of the input are of the one that does. */
    control(on: boolean): void;
    stop(): void;
}

/** Starts the video of `conn` on `canvas`. */
export function start(conn: Session, canvas: OffscreenCanvas): Video {
    const context = canvas.getContext("2d", { alpha: false, desynchronized: true }) as OffscreenCanvasRenderingContext2D;

    let decoder: VideoDecoder | undefined;
    let codec = "";
    let waitingForKey = true;
    let asked = 0;
    let screen = 0;
    let width = 0, height = 0;
    let frames = 0, bytes = 0, lastStats = performance.now();
    let captureMs = 0, encodeMs = 0, latencyMs = -1; // of the last frame, told each second
    let lastId = 0; // the last frame given to the decoder: a decoder that fails acknowledges it, the frames it held are lost
    const chunk: EncodedVideoChunkInit = { type: "key", timestamp: 0, data: new Uint8Array(0) };
    let hostAt: number | undefined;
    let displays: Display[] = [];
    let active = -1;
    let fitting = false;
    let fitW = 0, fitH = 0;
    let fittedScreen: number | undefined;
    let fitTimer: ReturnType<typeof setTimeout> | undefined;
    /** The frames tell the time of the input of the viewer that controls: of another one, it is not measured here. */
    let controlling = false;

    const askKey = () => {
        const now = performance.now();
        if (now - asked < 500) return;
        asked = now;
        Working.ONE.transmitter.send_Viewer_KeyframeRequest(conn);
    };

    /** The frames in the decoder are lost: acknowledged, else the machine waits for them and sends nothing more - the key frame
     *  asked for too. */
    const lost = () => {
        Working.ONE.transmitter.send_Viewer_VideoAck(lastId, conn);
        waitingForKey = true;
        askKey();
    };

    const newDecoder = () => {
        decoder?.state !== "closed" && decoder?.close();
        decoder = new VideoDecoder({
            output: frame => {
                const id = frame.timestamp;
                if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
                    canvas.width = frame.displayWidth;
                    canvas.height = frame.displayHeight;
                    toPage({ t: M.SIZE, w: canvas.width, h: canvas.height });
                }
                try {
                    context.drawImage(frame, 0, 0);
                } finally {
                    frame.close();
                }
                Working.ONE.transmitter.send_Viewer_VideoAck(id, conn);
                frames++;
            },
            error: e => {
                console.warn("the decoder failed:", e.message);
                decoder = undefined;
                codec = "";
                lost();
            },
        });
    };

    /** All of a frame but the input mark is in: it goes to the decoder. */
    const onFrame = (packet: Frame, data: Uint8Array) => {
        bytes += data.length;

        const format = packet.format;
        if (format !== undefined) {
            width = Viewer.Machine_.VideoFormat.get.width(format);
            height = Viewer.Machine_.VideoFormat.get.height(format);
        }
        if (packet.screen !== screen) {
            screen = packet.screen;
            toPage({ t: M.SCREEN, screen });
            if (fitting) fitSoon();
        }

        if (packet.keyframe) {
            const wanted = avcCodec(data);
            if (wanted && (wanted !== codec || !decoder || decoder.state === "closed")) {
                newDecoder();
                decoder!.configure({ codec: wanted, codedWidth: width, codedHeight: height, optimizeForLatency: true, hardwareAcceleration: "no-preference" });
                codec = wanted;
            }
            waitingForKey = false;
        } else if (waitingForKey || !decoder || decoder.state !== "configured") {
            Working.ONE.transmitter.send_Viewer_VideoAck(packet.frameId, conn); // dropped: it does not hold the machine back
            askKey();
            return;
        }

        lastId = packet.frameId;
        chunk.type = packet.keyframe ? "key" : "delta";
        chunk.timestamp = packet.frameId;
        chunk.data = data;
        try {
            decoder!.decode(new EncodedVideoChunk(chunk)); // the chunk copies the data: the receive buffer may go on
        } catch (e) {
            console.warn("a frame cannot be decoded:", (e as Error).message);
            lost();
        }
        captureMs = packet.captureMs;
        encodeMs = packet.encodeMs;
    };

    const frame = new Frame(onFrame);
    Viewer._Allocator.DEFAULT.new_Machine_VideoPacket = src => src === conn.receiver ? frame.next() : ignored.next();

    /** The whole frame is in: the input mark, the last field, too. Its time is the worker's: the page's input time is turned
     *  into it ( ./input.ts ). */
    const onPacket = (packet: Viewer.Machine_.VideoPacket, c: Session) => {
        const sentAt = (packet as Frame).eventMs;
        if (c === conn && sentAt !== undefined && controlling) latencyMs = ((performance.now() >>> 0) - sentAt) >>> 0;
    };

    /** The displays to choose from; the one the user of the machine works on is marked by the page. */
    const tellDisplays = () => toPage({ t: M.DISPLAYS, displays, active });

    const onLayout = (layout: Viewer.Machine_.VideoLayout, c: Session) => {
        if (c !== conn) return;
        displays = (layout.displays ?? []).map(d => ({ screen: d.screen, name: d.name ?? "", width: d.width, height: d.height }));
        tellDisplays();
    };

    const onActive = (display: Viewer.Machine_.ActiveDisplay, c: Session) => {
        if (c !== conn) return;
        active = display;
        tellDisplays();
    };

    // ── the resolution fitted to the page ──

    const fitNow = () => {
        fitTimer = undefined;
        if (!fitting) return;
        if (fittedScreen !== undefined && fittedScreen !== screen) restore(fittedScreen);
        if (fitW < 64 || fitH < 64) return;
        Working.ONE.transmitter.send_Viewer_ClientResolution(Viewer.Viewer_.ClientResolution.New.of(fitW, fitH, screen), conn);
        fittedScreen = screen;
    };
    /** The window settles first: a resize gives many sizes. */
    const fitSoon = () => {
        if (fitTimer !== undefined) clearTimeout(fitTimer);
        fitTimer = setTimeout(fitNow, 800);
    };
    const restore = (n: number) => Working.ONE.transmitter.send_Viewer_ClientResolution(Viewer.Viewer_.ClientResolution.New.of(0, 0, n), conn);

    // ── the pointer of the machine ──

    const onShape = (shape: Viewer.Machine_.CursorShape, c: Session) => {
        if (c !== conn) return;
        const bgra = shape.bgra?.view();
        if (!bgra) return;
        const rgba = new Uint32Array(shape.width * shape.height); // BGRA to RGBA a pixel at a time, little endian
        for (let i = 0, j = 0; i < rgba.length && j + 3 < bgra.length; i++, j += 4) rgba[i] = bgra[j + 2] | bgra[j + 1] << 8 | bgra[j] << 16 | bgra[j + 3] << 24;
        toPage({ t: M.CURSOR, w: shape.width, h: shape.height, x: shape.hotspot_x, y: shape.hotspot_y, rgba: rgba.buffer }, [rgba.buffer]);
    };

    const onHostCursor = (position: Viewer.Machine_.HostCursorPosition, c: Session) => {
        if (c !== conn) return;
        hostAt = position;
        const F = Viewer.FractionalCoordinate.get;
        if (position === undefined) toPage({ t: M.HOST_AT, x: 0, y: 0, screen: -1 }); // not on a display shown
        else toPage({ t: M.HOST_AT, x: F.x(position), y: F.y(position), screen: F.screen(position) });
    };

    const control = (pack: Viewer.Viewer_.VideoControl) => Working.ONE.transmitter.send_Viewer_VideoControl(pack, conn);

    Working.OnReceived.Machine_VideoPacket.handlers.subscribe(onPacket);
    Working.OnReceived.Machine_VideoLayout.handlers.subscribe(onLayout);
    Working.OnReceived.Machine_CursorShape.handlers.subscribe(onShape);
    Working.OnReceived.Machine_HostCursorPosition.handlers.subscribe(onHostCursor);
    Working.OnReceived.Machine_ActiveDisplay.handlers.subscribe(onActive);

    const timer = setInterval(() => {
        const now = performance.now();
        const seconds = (now - lastStats) / 1000;
        toPage({ t: M.STATS, w: width, h: height, fps: frames / seconds, mbps: bytes * 8 / seconds / 1e6, capture: captureMs, encode: encodeMs, latency: latencyMs });
        frames = 0;
        bytes = 0;
        lastStats = now;
    }, 1000);

    return {
        screen: () => screen,
        hostAt: () => hostAt,
        framerate(fps: number) {
            control(Viewer.Viewer_.VideoControl.set.framerate(Viewer.Viewer_.VideoControl.EMPTY_PACK, fps));
        },
        visible(on: boolean) {
            control(Viewer.Viewer_.VideoControl.set.enable(Viewer.Viewer_.VideoControl.EMPTY_PACK, on));
        },
        select(n: number) {
            Working.ONE.transmitter.send_Viewer_SelectDisplay(n, conn);
        },
        fit(on: boolean, now: boolean, w: number, h: number) {
            fitting = on;
            if (on) {
                fitW = w;
                fitH = h;
                return now ? fitNow() : fitSoon();
            }
            if (fitTimer !== undefined) clearTimeout(fitTimer);
            fitTimer = undefined;
            if (fittedScreen !== undefined) restore(fittedScreen);
            fittedScreen = undefined;
        },
        control(on: boolean) {
            controlling = on;
            if (on) return;
            // the fit is the machine's now: it restores it when the viewer that fitted it leaves while it controls, or the last one does
            fitting = false;
            fittedScreen = undefined;
            if (fitTimer !== undefined) clearTimeout(fitTimer);
            fitTimer = undefined;
            latencyMs = -1;
        },
        stop() {
            clearInterval(timer);
            if (fitTimer !== undefined) clearTimeout(fitTimer);
            Working.OnReceived.Machine_ActiveDisplay.handlers.unsubscribe(onActive);
            Working.OnReceived.Machine_VideoPacket.handlers.unsubscribe(onPacket);
            Working.OnReceived.Machine_VideoLayout.handlers.unsubscribe(onLayout);
            Working.OnReceived.Machine_CursorShape.handlers.unsubscribe(onShape);
            Working.OnReceived.Machine_HostCursorPosition.handlers.unsubscribe(onHostCursor);
            Viewer._Allocator.DEFAULT.new_Machine_VideoPacket = () => ignored.next();
            if (decoder && decoder.state !== "closed") decoder.close();
        },
    };
}

/**
 * `Machine.VideoPacket` as the worker takes it: one object for every frame, no array allocated for each. The fields come before
 * the bytes, the input mark after them: once the last bytes are in, the frame goes on - straight out of the receive buffer when it
 * came in one portion of it, else out of a buffer kept for the bytes and grown as the frames grow.
 */
class Frame extends Viewer.Machine_.VideoPacket {
    screen = 0;
    frameId = 0;
    keyframe = false;
    format: Viewer.Machine_.VideoFormat | undefined;
    captureMs = 0;
    encodeMs = 0;
    eventMs: number | undefined;
    private length = 0;
    private buffer = new Uint8Array(0);

    constructor(private readonly done: (frame: Frame, data: Uint8Array) => void) { super(); }

    /** For the next frame: the fields that may not come are cleared. */
    next(): Frame {
        this.format = undefined;
        this.eventMs = undefined;
        return this;
    }

    protected set _screen(src: number) { this.screen = src; }
    protected set _frame_id(src: number) { this.frameId = src; }
    protected set _keyframe(src: boolean) { this.keyframe = src; }
    protected set _format(src: Viewer.Machine_.VideoFormat.Nullable) { this.format = src as Viewer.Machine_.VideoFormat; } // only a present one is set
    protected set _capture_ms(src: number) { this.captureMs = src; }
    protected set _encode_ms(src: number) { this.encodeMs = src; }
    protected set _capture_pending_ms(_: number) {}
    protected set _capture_overhead_ms(_: number) {}
    protected set _encode_pending_ms(_: number) {}
    protected set _send_pending_ms(_: number) {}
    protected set _latest_event_ms(src: number | undefined) { this.eventMs = src; }

    protected _data_new(_: Base_.Receiver, __: Base_.Receiver.Slot, items_count: number): void {
        this.length = items_count;
        if (this.buffer.length < items_count) this.buffer = new Uint8Array(items_count + (items_count >> 2));
    }

    protected _dataˎ(_: Base_.Receiver, __: Base_.Receiver.Slot, item: number, src: Uint8Array): void {
        if (item === 0 && src.length === this.length) return this.done(this, src);
        this.buffer.set(src, item);
        if (item + src.length === this.length) this.done(this, this.buffer.subarray(0, this.length));
    }
}

/** The frames of a connection no video shows: taken in and dropped. */
const ignored = new Frame(() => {});
Viewer._Allocator.DEFAULT.new_Machine_VideoPacket = () => ignored.next();

/** `avc1.PPCCLL` of the sequence parameter set in the Annex B bytes of a key frame. */
function avcCodec(bytes: Uint8Array): string | undefined {
    for (let i = 0; i + 6 < bytes.length; i++) {
        if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1 && (bytes[i + 3] & 0x1f) === 7) {
            const hex = (b: number) => b.toString(16).padStart(2, "0");
            return "avc1." + hex(bytes[i + 4]) + hex(bytes[i + 5]) + hex(bytes[i + 6]);
        }
    }
    return undefined;
}
