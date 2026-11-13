// The sound of the session on the page: the AudioContext and its worklets live here, the packets in the worker of the session
// ( ./worker/sound.ts ). Each worklet talks to the worker over a MessageChannel of its own: the samples do not pass this thread.
// RAW 16-bit at 48 kHz: the machine's stereo, the microphone mono, in packets of 20 ms.
//
// Two worklets: the player keeps a short queue - it starts at `START_MS`; past `MAX_MS` it drops the oldest down to `START_MS`, so
// a burst does not leave the sound late for good; the microphone cuts what it hears into packets. The buffers of the samples go
// round - worker, worklet, worker - and are not allocated anew for each packet.

import { W, type ToWorker } from "./messages";

type Post = (message: ToWorker, transfer?: Transferable[]) => void;

const RATE = 48_000;

// the first message on the port of a worklet is the port to the worker: the samples go over that one
const WORKLETS = `
const START_MS = 60, MAX_MS = 150;
class Player extends AudioWorkletProcessor {
    constructor() {
        super();
        this.ring = new Int16Array(sampleRate * 2);
        this.read = 0; this.count = 0; this.playing = false;
        this.start = sampleRate * START_MS / 1000 * 2;
        this.max = sampleRate * MAX_MS / 1000 * 2;
        this.port.onmessage = e => {
            const worker = e.data;
            // a packet: into the ring, its buffer back to the worker - the next packet comes in it
            worker.onmessage = e => {
                const buffer = e.data, s = new Int16Array(buffer, 0, (buffer.byteLength >> 2) << 1), ring = this.ring, cap = ring.length;
                const w = (this.read + this.count) % cap, first = Math.min(s.length, cap - w);
                ring.set(first === s.length ? s : s.subarray(0, first), w);
                if (first < s.length) ring.set(s.subarray(first), 0);
                this.count += s.length;
                if (this.count > this.max) { const late = this.count - this.start; this.read = (this.read + late) % cap; this.count = this.start; }
                worker.postMessage(buffer, [buffer]);
            };
        };
    }
    process(_inputs, outputs) {
        const out = outputs[0], left = out[0], right = out[1] ?? out[0];
        if (!this.playing) {
            if (this.count < this.start) return true;
            this.playing = true;
        }
        const ring = this.ring, cap = ring.length;
        for (let i = 0; i < left.length; i++) {
            if (this.count < 2) { this.playing = false; break; } // starved: wait for START_MS again
            left[i] = ring[this.read] / 32768;
            right[i] = ring[this.read + 1] / 32768;
            this.read = this.read + 2 === cap ? 0 : this.read + 2;
            this.count -= 2;
        }
        return true;
    }
}
class Microphone extends AudioWorkletProcessor {
    constructor() {
        super();
        this.frames = sampleRate / 50; this.out = new Int16Array(this.frames); this.at = 0;
        this.free = []; // buffers the worker has sent: filled again
        this.worker = undefined;
        this.port.onmessage = e => {
            this.worker = e.data;
            this.worker.onmessage = e => this.free.push(e.data);
        };
    }
    process(inputs) {
        const ch = inputs[0] && inputs[0][0];
        if (!ch) return true;
        for (let i = 0; i < ch.length; i++) {
            const v = Math.max(-1, Math.min(1, ch[i]));
            this.out[this.at++] = v < 0 ? v * 32768 : v * 32767;
            if (this.at === this.frames) {
                this.at = 0;
                if (!this.worker) continue; // not connected yet: the packet is dropped, its buffer filled again
                this.worker.postMessage(this.out.buffer, [this.out.buffer]);
                const free = this.free.pop();
                this.out = free === undefined ? new Int16Array(this.frames) : new Int16Array(free);
            }
        }
        return true;
    }
}
registerProcessor("rdp-player", Player);
registerProcessor("rdp-microphone", Microphone);
`;

/** Joins `node` with the worker: one end of a new channel to the worklet, the other to the worker by a message `t`. */
function join(node: AudioWorkletNode, t: W.PLAYER | W.MIC, post: Post): void {
    const channel = new MessageChannel();
    node.port.postMessage(channel.port1, [channel.port1]);
    post({ t, port: channel.port2 }, [channel.port2]);
}

/** What the buttons and the notices of the sound say. */
export interface Words {
    sound(on: boolean): string;
    mic(on: boolean): string;
    noSound(error: string): string;
    micNotAllowed: string;
    micError(error: string): string;
    micFailed: string;
}

export interface Audio {
    /** The machine cannot play the microphone: it is off. */
    micFailed(): void;
    /** This viewer controls the machine, or not: only the microphone of the one that does plays there. */
    control(on: boolean): void;
    stop(): void;
}

/**
 * `soundButton` turns the sound of the machine on and off, `micButton` the microphone; `notice` tells the user what went wrong.
 */
export function start(soundButton: HTMLButtonElement, micButton: HTMLButtonElement, notice: (text: string) => void, words: Words, post: Post): Audio {
    const context = new AudioContext({ sampleRate: RATE, latencyHint: "interactive" });
    // a data: URL: a page opened from the disk may not load a worklet from a Blob URL, its origin is none
    const url = "data:text/javascript;charset=utf-8," + encodeURIComponent(WORKLETS);
    let player: AudioWorkletNode | undefined;
    let sound = true;
    let mic: { stream: MediaStream; node: AudioWorkletNode; source: MediaStreamAudioSourceNode; mute: GainNode } | undefined;
    let micWanted = false;
    let stopped = false;

    const ready = context.audioWorklet.addModule(url).then(() => {
        if (stopped) return;
        player = new AudioWorkletNode(context, "rdp-player", { numberOfInputs: 0, outputChannelCount: [2] });
        player.connect(context.destination);
        join(player, W.PLAYER, post);
    }).catch(e => notice(words.noSound((e as Error).message)));

    /** The browser plays sound only after the user did something on the page. */
    const wake = () => { if (context.state === "suspended") void context.resume(); };
    document.addEventListener("pointerdown", wake, true);
    document.addEventListener("keydown", wake, true);

    const label = () => {
        soundButton.textContent = words.sound(sound);
        micButton.textContent = words.mic(micWanted);
    };
    label();

    // ── the machine's sound ──

    const toggleSound = () => {
        sound = !sound;
        post({ t: W.SOUND, on: sound });
        label();
    };

    // ── the microphone ──

    /** The microphone of this device is let go; true - it was on. */
    const release = () => {
        if (!mic) return false;
        mic.stream.getTracks().forEach(t => t.stop());
        mic.source.disconnect();
        mic.node.disconnect();
        mic.mute.disconnect();
        mic = undefined;
        return true;
    };

    const micOff = (tell: boolean) => {
        micWanted = false;
        if (release() || tell) post({ t: W.MIC_OFF, tell });
        label();
    };

    const micOn = async () => {
        micWanted = true;
        label();
        wake();
        try {
            await ready;
            const stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
            if (!micWanted || stopped) return stream.getTracks().forEach(t => t.stop());
            const source = context.createMediaStreamSource(stream);
            const node = new AudioWorkletNode(context, "rdp-microphone", { numberOfInputs: 1, numberOfOutputs: 1, channelCount: 1, channelCountMode: "explicit" });
            const mute = context.createGain(); // the node is processed only when it leads to the output
            mute.gain.value = 0;
            source.connect(node).connect(mute).connect(context.destination);
            mic = { stream, node, source, mute };
            join(node, W.MIC, post); // the worker tells the machine the microphone is on
        } catch (e) {
            micOff(false);
            notice((e as Error).name === "NotAllowedError" ? words.micNotAllowed : words.micError((e as Error).message));
        }
    };

    const toggleMic = () => (micWanted ? micOff(true) : void micOn());

    soundButton.addEventListener("click", toggleSound);
    micButton.addEventListener("click", toggleMic);

    return {
        micFailed() {
            micOff(false);
            notice(words.micFailed);
        },
        control(on) {
            micButton.disabled = !on;
            if (!on && micWanted) micOff(false); // the machine stopped it
        },
        stop() {
            stopped = true;
            micWanted = false;
            release(); // the worker closed its ports with the session
            soundButton.removeEventListener("click", toggleSound);
            micButton.removeEventListener("click", toggleMic);
            micButton.disabled = false;
            document.removeEventListener("pointerdown", wake, true);
            document.removeEventListener("keydown", wake, true);
            void context.close();
        },
    };
}
