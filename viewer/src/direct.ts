// Way 4, direct: the page and the machine meet with no one between them - a data channel of WebRTC - and the two lines of the
// exchange go by hand, through any messenger ( host/src/direct.rs is the other side ).
//
// The page makes the offer: its line `rd1o.…` - its ICE name and password, the SHA-256 of its DTLS certificate, the addresses it
// is reached at - goes to the user of the machine; the answer of the machine `rd1a.…` - the same of the machine, and the token -
// comes back. The page takes it as the SDP the browser needs: the machine writes none.
//
// The page is the controlling side of ICE, the server of DTLS; the data channel is negotiated - id 0 on both sides, no DCEP -,
// and once open it goes to the worker of the session: the session runs over it as over a WebSocket, under its Noise.

/**
 * The public STUN servers that tell the page the address the internet sees it at. The browser asks them all at once and goes on
 * without the ones it does not reach - Google is closed in mainland China. Four: from five on a browser warns that its search
 * slows down. The program has a longer list ( host/src/direct.rs ).
 */
const STUN: RTCIceServer[] = [
    { urls: "stun:stun.l.google.com:19302" },                       // known all over the world
    { urls: "stun:stun.cloudflare.com:3478" },                      // the same
    { urls: "stun:stun.miwifi.com:3478" },                          // mainland China: Xiaomi
    { urls: "stun:stun.kinesisvideo.ap-east-1.amazonaws.com:443" }, // Hong Kong: Amazon, its region ap-east-1
];
/** The data channel of the session: negotiated, the same id on both sides. */
const CHANNEL = 0;
/** How long the page gathers its addresses at most. */
const GATHERING_MS = 4000;
/** How long the page waits for the machine once it has the answer: the machine keeps trying all along. */
const MEETING_MS = 60000;

export const OFFER_PREFIX = "rd1o.", ANSWER_PREFIX = "rd1a.";
const OFFER = 0x10, ANSWER = 0x11;

/** What one side tells the other. */
export interface Card {
    ufrag: string;
    pwd: string;
    /** SHA-256 of the DTLS certificate. */
    print: Uint8Array;
    candidates: { ip: string; port: number }[];
    /** The token: in the answer of the machine. */
    token: string;
    /**
     * The answer: the router of the machine lets in only what it asked for - a page whose network gives it a new port for each
     * address does not meet it. In the line: a byte of flags after the token, this one its bit 0; a machine before it sends none.
     */
    picky: boolean;
}

const ABC = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

function base64url(b: Uint8Array): string {
    let s = "";
    for (let i = 0; i < b.length; i += 3) {
        const n = b[i]! << 16 | (b[i + 1] ?? 0) << 8 | (b[i + 2] ?? 0), m = Math.min(3, b.length - i);
        for (let k = 0; k <= m; k++) s += ABC[n >> (18 - 6 * k) & 63];
    }
    return s;
}

function unbase64url(s: string): Uint8Array | undefined {
    const out: number[] = [];
    let acc = 0, bits = 0;
    for (const c of s) {
        const v = ABC.indexOf(c);
        if (v < 0) return undefined;
        acc = (acc << 6 | v) & 0xFFFFFF;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            out.push(acc >> bits & 255);
        }
    }
    return Uint8Array.from(out);
}

const encoder = new TextEncoder(), decoder = new TextDecoder();

/** The line of `card`: the answer of the machine, or the offer of the page. */
export function line(card: Card, answer: boolean): string {
    const b: number[] = [answer ? ANSWER : OFFER];
    const short = (s: string) => { const t = encoder.encode(s).subarray(0, 255); b.push(t.length, ...t); };
    short(card.ufrag);
    short(card.pwd);
    b.push(...card.print.subarray(0, 32));
    const cs = card.candidates.slice(0, 255);
    b.push(cs.length);
    for (const c of cs) {
        const ip = ipBytes(c.ip)!;
        b.push(ip.length === 4 ? 4 : 6, ...ip, c.port >> 8, c.port & 255);
    }
    if (answer) { short(card.token); b.push(card.picky ? 1 : 0); }
    return (answer ? ANSWER_PREFIX : OFFER_PREFIX) + base64url(Uint8Array.from(b));
}

/** The card of a line; undefined - not one, or not of this kind. */
export function card(text: string, answer: boolean): Card | undefined {
    const s = text.trim(), prefix = answer ? ANSWER_PREFIX : OFFER_PREFIX;
    if (!s.startsWith(prefix)) return undefined;
    const b = unbase64url(s.slice(prefix.length));
    if (b === undefined) return undefined;
    let at = 0;
    const byte = () => at < b.length ? b[at++]! : -1;
    const take = (n: number) => { if (at + n > b.length) return undefined; const v = b.subarray(at, at + n); at += n; return v; };
    const text_ = () => { const n = byte(); const v = n < 0 ? undefined : take(n); return v === undefined ? undefined : decoder.decode(v); };
    if (byte() !== (answer ? ANSWER : OFFER)) return undefined;
    const ufrag = text_(), pwd = text_(), print = take(32), n = byte();
    if (ufrag === undefined || pwd === undefined || print === undefined || n < 0) return undefined;
    const candidates: Card["candidates"] = [];
    for (let i = 0; i < n; i++) {
        const family = byte(), ip = take(family === 4 ? 4 : family === 6 ? 16 : -1), port = take(2);
        if (ip === undefined || port === undefined) return undefined;
        candidates.push({ ip: ipText(ip), port: port[0]! << 8 | port[1]! });
    }
    const token = answer ? text_() : "";
    const flags = answer ? byte() : -1; // -1, none: a machine before the flags
    const picky = flags > 0 && (flags & 1) === 1;
    return token === undefined ? undefined : { ufrag, pwd, print: print.slice(), candidates, token, picky };
}

function ipBytes(ip: string): number[] | undefined {
    if (/^\d+\.\d+\.\d+\.\d+$/.test(ip)) return ip.split(".").map(Number);
    if (!ip.includes(":")) return undefined;
    const [head, tail] = ip.split("::") as [string, string | undefined];
    const part = (s: string | undefined) => s ? s.split(":").map(h => parseInt(h, 16)) : [];
    const h = part(head), t = part(tail), groups = tail === undefined ? h : [...h, ...Array(8 - h.length - t.length).fill(0), ...t];
    return groups.length === 8 ? groups.flatMap(g => [g >> 8, g & 255]) : undefined;
}

function ipText(b: Uint8Array): string {
    if (b.length === 4) return Array.from(b).join(".");
    const g: string[] = [];
    for (let i = 0; i < 16; i += 2) g.push((b[i]! << 8 | b[i + 1]!).toString(16));
    return g.join(":");
}

const hex2 = (b: number) => b.toString(16).toUpperCase().padStart(2, "0");

/** The card of the SDP of the page: what the browser wrote of itself; the addresses hidden behind names of mDNS are left out. */
function ofSdp(sdp: string): Card | undefined {
    const value = (name: string) => sdp.match(new RegExp(`^a=${name}:(.+)$`, "m"))?.[1]?.trim();
    const ufrag = value("ice-ufrag"), pwd = value("ice-pwd"), fp = value("fingerprint");
    if (ufrag === undefined || pwd === undefined || fp === undefined || !fp.toLowerCase().startsWith("sha-256 ")) return undefined;
    const print = Uint8Array.from(fp.slice(8).trim().split(":").map(h => parseInt(h, 16)));
    const candidates: Card["candidates"] = [];
    for (const m of sdp.matchAll(/^a=candidate:\S+ \d+ (\S+) \d+ (\S+) (\d+) typ (\S+)/gm)) {
        const [, proto, ip, port] = m;
        if (proto!.toLowerCase() !== "udp" || ipBytes(ip!) === undefined) continue;
        if (!candidates.some(c => c.ip === ip && c.port === +port!)) candidates.push({ ip: ip!, port: +port! });
    }
    return print.length === 32 ? { ufrag, pwd, print, candidates, token: "", picky: false } : undefined;
}

/** The SDP of the answer of the machine, as the browser takes it: one m-line, the data channel, the `mid` of the offer. */
function answerSdp(c: Card, mid: string): string {
    const lines = [
        "v=0", "o=- 0 0 IN IP4 127.0.0.1", "s=-", "t=0 0", `a=group:BUNDLE ${mid}`, "a=msid-semantic: WMS",
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel", "c=IN IP4 0.0.0.0",
        `a=ice-ufrag:${c.ufrag}`, `a=ice-pwd:${c.pwd}`,
        `a=fingerprint:sha-256 ${Array.from(c.print, hex2).join(":")}`,
        "a=setup:active", `a=mid:${mid}`, "a=sctp-port:5000", "a=max-message-size:262144",
        ...c.candidates.map((a, i) => `a=candidate:${i + 1} 1 udp ${2122260223 - i * 256} ${a.ip} ${a.port} typ host`),
        "a=end-of-candidates",
    ];
    return lines.join("\r\n") + "\r\n";
}

/**
 * The data channel to the worker of the session: a browser lets a data channel go to a worker only at its creation, and this one
 * was made long before - so the page keeps it and passes the bytes. Each message goes over `port` as its buffer, moved, not
 * copied; `undefined` - closed, either way.
 */
export function relay(channel: RTCDataChannel, port: MessagePort): void {
    channel.binaryType = "arraybuffer";
    channel.onmessage = e => { const b = e.data as ArrayBuffer; port.postMessage(b, [b]); };
    channel.onclose = () => { port.postMessage(undefined); port.close(); };
    port.onmessage = e => {
        if (e.data === undefined) channel.close();
        else if (channel.readyState === "open") channel.send(e.data as ArrayBuffer);
    };
}

/** A direct connection being made: the line of the page to send, the answer of the machine to take. */
export interface Offer {
    /** The line of the page, `rd1o.…`. */
    readonly line: string;
    /** The network of this device gives it a new port for each address it talks to. */
    readonly strict: boolean;
    /** Takes the answer of the machine: the data channel once open, and the token of the machine; rejects - the two did not meet. */
    finish(card: Card): Promise<RTCDataChannel>;
    /** Gives the attempt up. */
    close(): void;
}

/** The STUN servers the page asks: their names, as `heard` names them. */
export const STUN_NAMES: readonly string[] = STUN.map(s => nameOf(String(s.urls)));

function nameOf(url: string): string {
    return url.replace(/^stuns?:/, "").replace(/:\d+$/, "");
}

/**
 * What the page learns on its way, told as it learns it, for a page that draws it: `ask` - the STUN servers are asked; `local` - an
 * address of this device itself; `seen` - a STUN server answered, the address the internet sees this device at, in `ms` from the
 * start; `silent` - a STUN server did not answer; `gathered` - the line is made; `strict` - the network of this device gives it a
 * new port for each address it talks to; `state` - how the meeting goes, as ICE says; `checked` - the meeting failed: the checks
 * sent to one address of the machine and the answers; `met` - the way found: the address of this side and of the machine, `how` -
 * through the internet or in one network.
 */
export type Heard =
    | { k: "ask"; servers: readonly string[] }
    | { k: "local"; address: string }
    | { k: "seen"; server: string; address: string; ms: number }
    | { k: "silent"; server: string; why: string }
    | { k: "gathered"; ms: number; seen: number }
    | { k: "state"; state: RTCIceConnectionState }
    | { k: "strict" }
    | { k: "checked"; address: string; sent: number; answered: number }
    | { k: "met"; mine: string; theirs: string; how: "internet" | "network"; ms: number };

/** The offer of the page: its addresses gathered, its line made; `heard` is told each step of the way. */
export async function offer(heard: (h: Heard) => void = () => undefined): Promise<Offer> {
    const pc = new RTCPeerConnection({ iceServers: STUN });
    const channel = pc.createDataChannel("rdp", { negotiated: true, id: CHANNEL, ordered: true });
    channel.binaryType = "arraybuffer";
    const start = performance.now(), answered = new Set<string>(), silent = new Set<string>();
    const ports = new Map<string, Set<number>>(); // the IPv4 the internet sees this device at: the ports the STUN servers saw
    heard({ k: "ask", servers: STUN_NAMES });
    pc.addEventListener("icecandidate", e => {
        const c = e.candidate;
        if (c === null || c.address == null || c.port == null || c.protocol !== "udp") return;
        const address = (c.address.includes(":") ? `[${c.address}]` : c.address) + ":" + c.port;
        if (c.type === "host") heard({ k: "local", address: c.address.endsWith(".local") ? "a hidden name ( mDNS )" : address });
        else if (c.type === "srflx") {
            // the server that answered: the event tells it in Chromium and Firefox, the candidate in the newer browsers
            const url = (e as { url?: string | null }).url ?? (c as { url?: string | null }).url;
            const server = url ? nameOf(url) : "a STUN server";
            answered.add(server);
            if (!c.address.includes(":")) {
                const seen = ports.get(c.address) ?? new Set<number>();
                ports.set(c.address, seen.add(c.port));
            }
            heard({ k: "seen", server, address, ms: Math.round(performance.now() - start) });
        }
    });
    pc.addEventListener("icecandidateerror", e => {
        const server = nameOf(e.url);
        if (answered.has(server) || silent.has(server) || !STUN_NAMES.includes(server)) return;
        silent.add(server);
        heard({ k: "silent", server, why: e.errorText || "no answer" });
    });
    await pc.setLocalDescription(await pc.createOffer());
    await new Promise<void>(done => {
        if (pc.iceGatheringState === "complete") return done();
        const timer = setTimeout(done, GATHERING_MS);
        pc.addEventListener("icegatheringstatechange", () => {
            if (pc.iceGatheringState === "complete") { clearTimeout(timer); done(); }
        });
    });
    for (const server of STUN_NAMES) {
        if (answered.has(server) || silent.has(server)) continue;
        silent.add(server); // its own error may come later, at the meeting: told once
        heard({ k: "silent", server, why: "no answer in time" });
    }
    heard({ k: "gathered", ms: Math.round(performance.now() - start), seen: answered.size });
    // one IP, a port of its own for each server: a symmetric NAT, as the ones of mobile operators
    const strict = [...ports.values()].some(p => p.size > 1);
    if (strict) heard({ k: "strict" });
    const sdp = pc.localDescription?.sdp ?? "";
    const mine = ofSdp(sdp);
    if (mine === undefined) {
        pc.close();
        throw new Error("the browser gave no description of its connection");
    }
    const mid = sdp.match(/^a=mid:(\S+)$/m)?.[1] ?? "0";
    return {
        line: line(mine, false),
        strict,
        async finish(c: Card): Promise<RTCDataChannel> {
            const meeting = performance.now();
            pc.addEventListener("iceconnectionstatechange", () => {
                heard({ k: "state", state: pc.iceConnectionState });
                if (pc.iceConnectionState === "connected" || pc.iceConnectionState === "completed") void way(pc).then(w => {
                    if (w !== undefined) heard({ k: "met", ...w, ms: Math.round(performance.now() - meeting) });
                });
            });
            await pc.setRemoteDescription({ type: "answer", sdp: answerSdp(c, mid) });
            if (channel.readyState === "open") return channel;
            return new Promise<RTCDataChannel>((resolve, reject) => {
                // what ICE counted, taken each second: once it gives up, the browser drops the pairs from its statistics
                let counted: Checks = { to: new Map(), came: 0 };
                const counting = setInterval(() => void count(pc).then(c => { if (c.to.size > 0 || c.came > 0) counted = c; }), 1000);
                const end = () => { clearTimeout(timer); clearInterval(counting); };
                const fail = () => {
                    end();
                    reject(new Error(why(counted, strict, heard)));
                };
                const timer = setTimeout(fail, MEETING_MS);
                channel.addEventListener("open", () => { end(); resolve(channel); });
                pc.addEventListener("connectionstatechange", () => {
                    if (pc.connectionState === "failed") fail();
                });
            });
        },
        close() { pc.close(); },
    };
}

/** What ICE counted: the checks sent to each address of the machine and the answers to them; the checks of the machine that came. */
interface Checks {
    to: Map<string, { sent: number; answered: number }>;
    came: number;
}

async function count(pc: RTCPeerConnection): Promise<Checks> {
    const stats = await pc.getStats();
    const to = new Map<string, { sent: number; answered: number }>();
    let came = 0;
    stats.forEach(s => {
        if (s.type !== "candidate-pair") return;
        came += s.requestsReceived ?? 0;
        const r = stats.get(s.remoteCandidateId);
        if (r === undefined || !s.requestsSent) return;
        const ip: string = r.address ?? r.ip ?? "?";
        const at = (ip.includes(":") ? `[${ip}]` : ip) + ":" + r.port;
        const t = to.get(at) ?? { sent: 0, answered: 0 };
        t.sent += s.requestsSent;
        t.answered += s.responsesReceived ?? 0;
        to.set(at, t);
    });
    return { to, came };
}

/**
 * Why the two did not meet, from what ICE counted: each address of the machine told to `heard` with its checks. `strict` - the
 * network of this device changes the port for each address.
 */
function why({ to, came }: Checks, strict: boolean, heard: (h: Heard) => void): string {
    for (const [address, t] of to) heard({ k: "checked", address, ...t });
    if ([...to.values()].some(t => t.answered > 0))
        return "the addresses met, the encrypted channel did not open: the remote computer may have given this meeting up - make a new line";
    if (came > 0)
        return "the checks of the remote computer came, none of its answers did: something on the way lets only one direction through - use way 3";
    return strict
        ? "the remote computer answered none of the checks: its router lets in only what it asked for, and the network of this device " +
          "gives it a new port for each address ( a strict NAT, as mobile operators have ) - such two meet only through way 3, " +
          "a port open to the remote computer on its router, or this device in another network ( Wi-Fi )"
        : "the remote computer answered none of the checks: its router, or a firewall on it, lets in nothing it did not ask for - use way 3, " +
          "or open a port to it on its router";
}

/** The pair of addresses ICE took: this side's and the machine's, and whether it goes through the internet or stays in one network. */
async function way(pc: RTCPeerConnection): Promise<{ mine: string; theirs: string; how: "internet" | "network" } | undefined> {
    const stats = await pc.getStats();
    let pairId: string | undefined;
    stats.forEach(s => { if (s.type === "transport" && s.selectedCandidatePairId) pairId = s.selectedCandidatePairId as string; });
    const pair = pairId === undefined ? undefined : stats.get(pairId);
    const local = pair && stats.get(pair.localCandidateId), remote = pair && stats.get(pair.remoteCandidateId);
    if (local === undefined || remote === undefined) return undefined;
    const ip = (c: { address?: string; ip?: string }) => c.address ?? c.ip ?? "?";
    const at = (c: { address?: string; ip?: string; port: number }) => ip(c) + ":" + c.port;
    const inside = /^(10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.|169\.254\.|f[cd]|fe80:)/i.test(ip(remote));
    return { mine: at(local), theirs: at(remote), how: local.candidateType === "host" && inside ? "network" : "internet" };
}
