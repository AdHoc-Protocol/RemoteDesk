// The sound of the session in the worker: RAW 16-bit at 48 kHz, the machine's stereo, the microphone mono, in packets of 20 ms. The
// worklets live on the page ( an AudioContext is only there, ../audio.ts ), each talks to the worker over a port of its own: the
// samples do not pass the thread of the page. The buffers of the samples go round - worker, worklet, worker - and are not
// allocated anew for each packet.

import Viewer from "../../protocol/gen/Viewer";
import Session from "../../protocol/gen/Session/Connection";
import Actor0 from "../../protocol/gen/Session/Actor0";
import Base_ from "../../protocol/gen/Base_";
import { M } from "../messages";
import { toPage } from "./page";

const Working = Actor0.State.Working;

/** Buffers and packets kept for the next ones, at most. */
const KEPT = 8;

/**
 * `AudioPacket` as the worker takes and sends it: no pack and no array allocated for each. Taken: its bytes go from the receive
 * buffer straight into a buffer the player gave back, and that buffer moves on to the player. Sent: it carries the buffer the
 * microphone filled, the transmit buffer is filled from it, and it goes back to the microphone.
 */
class Sound extends Viewer.AudioPacket {
    samples = new ArrayBuffer(0);
    private bytes = new Uint8Array(0);

    /** `heard` - the packet is in; `buffers` - those to take it in, given back by the one that heard it. */
    constructor(private readonly heard?: (packet: Sound) => void, private readonly buffers: ArrayBuffer[] = []) { super(); }

    /** To send `samples`: 16-bit mono, 48 kHz. */
    carry(samples: ArrayBuffer): Sound {
        this.samples = samples;
        this.bytes = new Uint8Array(samples);
        return this;
    }

    protected get _data(): object { return this.samples; }
    protected get _data_len(): number { return this.bytes.length; }
    protected _data̍(_: Base_.Transmitter, __: Base_.Transmitter.Slot, item: number, dst: Uint8Array): void {
        dst.set(item === 0 && dst.length === this.bytes.length ? this.bytes : this.bytes.subarray(item, item + dst.length));
    }

    protected _data_new(_: Base_.Receiver, __: Base_.Receiver.Slot, items_count: number): void {
        const free = this.buffers.pop();
        this.samples = free !== undefined && free.byteLength === items_count ? free : new ArrayBuffer(items_count);
        this.bytes = new Uint8Array(this.samples);
    }
    protected _dataˎ(_: Base_.Receiver, __: Base_.Receiver.Slot, item: number, src: Uint8Array): void {
        this.bytes.set(src, item);
        if (item + src.length === this.bytes.length) this.heard?.(this);
    }
}

/** The sound of a connection nobody listens to: taken in and dropped. */
const ignored = new Sound();
Viewer._Allocator.DEFAULT.new_AudioPacket = () => ignored;

export interface Audio {
    /** The port of the player: the sound of the machine goes there from now on. */
    player(port: MessagePort): void;
    /** The sound of the machine on or off. */
    sound(on: boolean): void;
    /** The port of the microphone: it is on. */
    mic(port: MessagePort): void;
    /** The microphone is off; `tell` - the machine is told so. */
    micOff(tell: boolean): void;
    stop(): void;
}

export function start(conn: Session): Audio {
    let player: MessagePort | undefined;
    let on = true;
    let mic: MessagePort | undefined;

    // ── the machine's sound ──

    const played: ArrayBuffer[] = []; // buffers the player is done with: the machine's next packets come in them
    const heard = (packet: Sound) => {
        if (!player || !on) return;
        player.postMessage(packet.samples, [packet.samples]);
    };
    const receiving = new Sound(heard, played);
    Viewer._Allocator.DEFAULT.new_AudioPacket = src => src === conn.receiver ? receiving : ignored;

    // ── the microphone ──

    /** A packet of the microphone is on the wire, or never got there: its buffer goes back to the microphone, the packet waits for
     *  the next samples. */
    const sending: Sound[] = [];
    const sent = (packet: Sound) => {
        if (mic) mic.postMessage(packet.samples, [packet.samples]);
        if (sending.length < KEPT) sending.push(packet);
    };
    const onSoundSent = (packet: Viewer.AudioPacket, c: Session) => { if (c === conn) sent(packet as Sound); };

    const micOff = (tell: boolean) => {
        mic?.close();
        mic = undefined;
        if (tell) Working.ONE.transmitter.send_Viewer_MicrophoneControl(false, conn);
    };

    const onMicError = (c: Session) => {
        if (c !== conn) return;
        micOff(false);
        toPage({ t: M.MIC_ERROR });
    };

    Working.OnSerialized.AudioPacket.handlers.subscribe(onSoundSent);
    Working.OnReceived.Machine_MicrophoneError.handlers.subscribe(onMicError);

    return {
        player(port: MessagePort) {
            player?.close();
            player = port;
            port.onmessage = (e: MessageEvent<ArrayBuffer>) => { if (played.length < KEPT) played.push(e.data); };
        },
        sound(value: boolean) {
            on = value;
            Working.ONE.transmitter.send_Viewer_AudioControl(on, conn);
        },
        mic(port: MessagePort) {
            mic?.close();
            mic = port;
            port.onmessage = (e: MessageEvent<ArrayBuffer>) => {
                const packet = (sending.pop() ?? new Sound()).carry(e.data);
                if (!Working.ONE.transmitter.send_AudioPacket(packet, conn)) sent(packet);
            };
            Working.ONE.transmitter.send_Viewer_MicrophoneControl(true, conn);
        },
        micOff,
        stop() {
            micOff(false);
            player?.close();
            player = undefined;
            Working.OnSerialized.AudioPacket.handlers.unsubscribe(onSoundSent);
            Working.OnReceived.Machine_MicrophoneError.handlers.unsubscribe(onMicError);
            Viewer._Allocator.DEFAULT.new_AudioPacket = () => ignored;
        },
    };
}
