// The diagram of the login page, drawn by hand ( Rough.js ): this computer and the remote one, their routers, what is between them -
// a cloud of the internet, a meeting point above it, or one house for one network -, the STUN servers of way 4, and the two paths of
// what goes from person to person, by chat or mail.
//
// The scene is drawn once per way; what it shows of the moment - waited for, on its way, alive, broken - is in the attributes of the
// diagram ( data-me, data-wa, data-out… ), and the style of the page makes of them the colours, the pulses, the flying paper. A part
// with an id here is one the page names: the paths the pulses run along, the racks of the STUN servers, the number pressed.

import rough from "roughjs";

const NS = "http://www.w3.org/2000/svg";
/** The size of the scene, in its own units: the page scales it to its width. */
export const WIDTH = 1000, HEIGHT = 680;

type Attrs = Record<string, string | number>;

function el<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Attrs = {}, parent?: Element, text?: string): SVGElementTagNameMap[K] {
    const e = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
    if (text !== undefined) e.textContent = text;
    parent?.append(e);
    return e;
}

/** Text written by hand. */
const write = (parent: Element, x: number, y: number, text: string, size = 22, cls = "") =>
    el("text", { x, y, "font-size": size, "text-anchor": "middle", class: "hand " + cls }, parent, text);

/** Pulses running along `path`: `n` each way, `back` - the other way too; the style shows them in the states that have them. */
function pulses(parent: Element, path: string, n: number, seconds: number, back: boolean): void {
    for (const reverse of back ? [false, true] : [false]) for (let i = 0; i < n; i++) {
        const dot = el("circle", { r: 5, class: "dot " + (reverse ? "back" : "fwd") }, parent);
        const motion = el("animateMotion", { dur: `${seconds}s`, begin: `${(-seconds * i) / n - (reverse ? seconds / (2 * n) : 0)}s`, repeatCount: "indefinite" }, dot);
        if (reverse) { motion.setAttribute("keyPoints", "1;0"); motion.setAttribute("keyTimes", "0;1"); motion.setAttribute("calcMode", "linear"); }
        el("mpath", { href: "#" + path }, motion);
    }
}

/** What runs along a path, drawn: an `id` of the page for the moving thing. */
function traveller(parent: Element, path: string, seconds: number, draw: (g: SVGGElement) => void, cls: string): void {
    const g = el("g", { class: cls }, parent);
    draw(g);
    const motion = el("animateMotion", { dur: `${seconds}s`, repeatCount: "indefinite", rotate: "auto" }, g);
    el("mpath", { href: "#" + path }, motion);
}

/** The cloud of the internet around (`cx`, `cy`), `w` × `h`: bumps all around. */
function cloudPath(cx: number, cy: number, w: number, h: number): string {
    const bumps: [number, number, number][] = [
        [-0.42, 0.18, 0.22], [-0.3, -0.12, 0.26], [-0.08, -0.32, 0.3], [0.2, -0.26, 0.27], [0.4, -0.02, 0.22], [0.33, 0.24, 0.2],
        [0.08, 0.32, 0.22], [-0.2, 0.32, 0.22],
    ];
    const pts = bumps.map(([x, y, r]) => [cx + x * w, cy + y * h, r * Math.min(w, h * 1.4)] as const);
    let d = "";
    pts.forEach(([x, y, r], i) => {
        const [nx, ny] = pts[(i + 1) % pts.length]!;
        if (i === 0) d += `M ${x} ${y + r * 0.6} `;
        d += `Q ${(x + nx) / 2 + (x + nx - 2 * cx) * 0.35} ${(y + ny) / 2 + (y + ny - 2 * cy) * 0.55} ${nx} ${ny + r * 0.1} `;
    });
    return d + "Z";
}

/** Draws the scene of `way` into `svg`, whatever was there gone. */
export function drawScene(svg: SVGSVGElement, way: 1 | 2 | 3 | 4): void {
    svg.replaceChildren();
    svg.setAttribute("viewBox", `0 0 ${WIDTH} ${HEIGHT}`);
    const rc = rough.svg(svg);
    const pen = { roughness: 1.3, bowing: 1.1, strokeWidth: 2.2, stroke: "#333" };
    /** An outline by hand: its colour is the style's, by the state of the part it is in. */
    const o = <T extends SVGGElement>(g: Element, shape: T, cls = "o") => { shape.classList.add(...cls.split(" ")); g.append(shape); return shape; };
    const defs = el("defs", {}, svg);

    // ── what goes from person to person: under the scene, two long paths around it ──
    const chat = el("g", { id: "chat" }, svg);
    const outD = "M 70 300 C 70 390 90 455 160 455 L 840 455 C 910 455 930 400 930 335";
    const backD = "M 965 335 C 968 520 950 605 860 605 L 140 605 C 50 605 35 470 38 305";
    el("path", { id: "out-path", d: outD, class: "lane-path" }, defs);
    el("path", { id: "back-path", d: backD, class: "lane-path" }, defs);
    const out = el("g", { id: "lane-out", class: "lane" }, chat);
    o(out, rc.path(outD, { ...pen, strokeWidth: 2, strokeLineDash: [9, 9], seed: 11 }));
    o(out, rc.polygon([[918, 345], [930, 322], [942, 345]], { ...pen, fill: "#333", fillStyle: "solid", seed: 12 }), "o head");
    traveller(out, "out-path", 5, g => {
        // a paper plane, its line in it
        o(g, rc.polygon([[-16, -9], [16, 0], [-16, 9], [-9, 0]], { ...pen, strokeWidth: 1.6, fill: "#fff", fillStyle: "solid", seed: 13 }), "o plane");
        o(g, rc.line(-9, 0, 16, 0, { ...pen, strokeWidth: 1.2, seed: 14 }), "o plane");
    }, "flyer");
    const back = el("g", { id: "lane-back", class: "lane" }, chat);
    o(back, rc.path(backD, { ...pen, strokeWidth: 2, strokeLineDash: [9, 9], seed: 21 }));
    o(back, rc.polygon([[26, 318], [38, 295], [50, 318]], { ...pen, fill: "#333", fillStyle: "solid", seed: 22 }), "o head");
    traveller(back, "back-path", 6, g => {
        // an envelope, the answer in it
        o(g, rc.rectangle(-15, -10, 30, 20, { ...pen, strokeWidth: 1.6, fill: "#fff", fillStyle: "solid", seed: 23 }), "o plane");
        o(g, rc.linearPath([[-15, -10], [0, 2], [15, -10]], { ...pen, strokeWidth: 1.4, seed: 24 }), "o plane");
    }, "flyer");

    // ── the middle: one house for one network; else the cloud of the internet: whatever goes between the two goes through it ──
    const mid = el("g", { id: "n-mid", class: "node" }, svg);
    if (way === 1) {
        o(mid, rc.polygon([[22, 100], [500, 8], [978, 100]], { ...pen, strokeWidth: 2.4, seed: 31 }));
        o(mid, rc.rectangle(40, 100, 920, 238, { ...pen, strokeWidth: 2.4, seed: 32 }));
        write(mid, 500, 76, "one home, one office: one network", 26, "label");
    } else {
        o(mid, rc.path(cloudPath(500, 222, 250, 150), { ...pen, strokeWidth: 2.4, fill: "#9cc4f0", fillStyle: "hachure", hachureGap: 9, hachureAngle: -30, seed: 33 }), "o cloud");
        write(mid, 500, way === 3 ? 274 : 214, "Internet", way === 3 ? 30 : 34, "label big"); // way 3: under the lines that go up
    }

    // ── the STUN servers of way 4: above the cloud, asked by this computer through it ──
    if (way === 4) {
        const names = ["Google", "Cloudflare", "Xiaomi", "Amazon HK"];
        const stun = el("g", { id: "stun" }, svg);
        names.forEach((name, i) => {
            const x = 318 + i * 100, y = 14;
            const rack = el("g", { id: `stun-${i}`, class: "rack" }, stun);
            const ping = `M 330 238 C 430 236 ${x + 26} 210 ${x + 26} ${y + 56}`;
            el("path", { id: `ping-${i}`, d: ping, class: "ping" }, rack);
            pulses(rack, `ping-${i}`, 1, 1.4, true);
            o(rack, rc.rectangle(x, y, 52, 54, { ...pen, strokeWidth: 1.8, fill: "#ddd", fillStyle: "solid", seed: 40 + i }), "o box");
            for (let k = 0; k < 3; k++) {
                o(rack, rc.line(x + 8, y + 13 + k * 14, x + 34, y + 13 + k * 14, { ...pen, strokeWidth: 1.4, seed: 50 + i * 3 + k }));
                el("circle", { cx: x + 43, cy: y + 13 + k * 14, r: 2.6, class: "led" }, rack);
            }
            write(rack, x + 26, y + 76, name, 18, "small");
            write(rack, x + 58, y + 8, "✓", 26, "mark ok");
            write(rack, x + 58, y + 8, "✕", 26, "mark no");
        });
    }

    // ── the connection: this computer, its router, through the internet - up to the meeting point and down again, on way 3 -, the
    // other router, the remote computer; on way 1, through the router of the house ──
    const [aD, bD] = way === 1
        ? ["M 270 286 C 330 290 400 262 468 252", "M 532 252 C 610 252 690 222 748 214"]
        : way === 3
            ? ["M 270 286 C 280 284 282 252 296 252 L 358 252 C 420 252 466 236 474 186 C 480 146 484 112 486 86",
               "M 514 86 C 516 112 520 146 526 186 C 534 236 580 252 642 252 L 704 252 C 722 252 730 220 748 214"]
            : ["M 270 286 C 280 284 282 252 296 252 L 358 252 C 400 252 440 258 500 258", "M 500 258 C 560 258 600 252 642 252 L 704 252 C 722 252 730 220 748 214"];
    for (const [id, d] of [["w-a", aD], ["w-b", bD]] as const) {
        const seg = el("g", { id, class: "seg" }, svg);
        el("path", { id: id + "-path", d, class: "live" }, defs);
        o(seg, rc.path(d, { ...pen, strokeWidth: 2.6, seed: id === "w-a" ? 61 : 62 }), "o base");
        el("use", { href: `#${id}-path`, class: "glow" }, seg);
        pulses(seg, id + "-path", 3, 2.2, true);
    }

    // ── the routers: one for each house; one for both in way 1; the meeting point above the internet for way 3 ──
    const router = (g: Element, x: number, y: number, label: string, seed: number, wifi = false) => {
        o(g, rc.line(x + 12, y, x + 5, y - 22, { ...pen, strokeWidth: 2, seed }));
        o(g, rc.line(x + 52, y, x + 59, y - 22, { ...pen, strokeWidth: 2, seed: seed + 1 }));
        o(g, rc.rectangle(x, y, 64, 26, { ...pen, fill: "#e8e8e8", fillStyle: "solid", seed: seed + 2 }), "o box");
        for (let i = 0; i < 3; i++) el("circle", { cx: x + 16 + i * 11, cy: y + 13, r: 3, class: `led led-${i}` }, g);
        if (wifi) for (let i = 1; i <= 3; i++)
            o(g, rc.arc(x + 32, y - 8, 22 * i, 22 * i, Math.PI * 1.2, Math.PI * 1.8, false, { ...pen, strokeWidth: 1.8, seed: seed + 3 + i }), "o wifi");
        write(g, x + 32, y + 50, label, 19, "small");
    };
    if (way === 1) router(el("g", { id: "r-mid", class: "router" }, mid), 468, 238, "router + Wi-Fi", 70, true);
    else {
        router(el("g", { id: "r-me", class: "router" }, svg), 296, 238, "your router", 72);
        router(el("g", { id: "r-rem", class: "router" }, svg), 642, 238, "its router", 76);
    }
    if (way === 3) {
        router(el("g", { id: "r-meet", class: "router" }, mid), 468, 60, "", 80);
        write(mid, 610, 82, "meeting point", 24, "label");
    }

    // ── this computer: a laptop, this page on its screen ──
    const me = el("g", { id: "n-me", class: "node" }, svg);
    write(me, 160, 136, "You", 30, "label big");
    o(me, rc.rectangle(60, 146, 196, 130, { ...pen, fill: "#fff", fillStyle: "solid", seed: 81 }), "o card");
    o(me, rc.rectangle(72, 158, 172, 106, { ...pen, strokeWidth: 1.6, fill: "#cfe0f7", fillStyle: "hachure", hachureGap: 7, seed: 82 }), "o screen");
    o(me, rc.line(72, 172, 244, 172, { ...pen, strokeWidth: 1.4, seed: 83 }));
    for (let i = 0; i < 3; i++) el("circle", { cx: 80 + i * 8, cy: 165, r: 2.2, class: "pip" }, me);
    write(me, 158, 222, "RemoteDesk", 24, "screen-text");
    o(me, rc.polygon([[44, 278], [272, 278], [292, 298], [24, 298]], { ...pen, fill: "#bbb", fillStyle: "zigzag", hachureGap: 6, seed: 84 }), "o deck");
    o(me, rc.rectangle(135, 284, 46, 9, { ...pen, strokeWidth: 1.2, seed: 85 }));

    // ── the remote computer: a monitor, its keyboard and mouse; the window of rdp-host on the screen, the number pressed ──
    const rem = el("g", { id: "n-rem", class: "node" }, svg);
    write(rem, 855, 126, "Remote computer", 30, "label big");
    o(rem, rc.rectangle(748, 134, 214, 142, { ...pen, fill: "#fff", fillStyle: "solid", seed: 91 }), "o card");
    o(rem, rc.rectangle(760, 146, 190, 112, { ...pen, strokeWidth: 1.6, fill: "#20242c", fillStyle: "solid", seed: 92 }), "o console");
    el("text", { x: 770, y: 170, class: "console-text" }, rem, "> rdp-host");
    el("text", { x: 770, y: 192, class: "console-text" }, rem, "How will the viewer");
    el("text", { x: 770, y: 208, class: "console-text" }, rem, "reach this computer?");
    const pressed = el("text", { x: 770, y: 236, class: "console-text strong" }, rem, `Pressed: ${way}`);
    el("rect", { x: 856, y: 226, width: 8, height: 13, class: "cursor" }, rem);
    pressed.id = "pressed";
    o(rem, rc.polygon([[843, 276], [867, 276], [873, 298], [837, 298]], { ...pen, fill: "#bbb", fillStyle: "solid", seed: 93 }), "o deck");
    o(rem, rc.ellipse(855, 300, 96, 12, { ...pen, fill: "#bbb", fillStyle: "solid", seed: 94 }), "o deck");
    o(rem, rc.polygon([[768, 308], [938, 308], [950, 326], [756, 326]], { ...pen, fill: "#bbb", fillStyle: "zigzag", hachureGap: 6, seed: 95 }), "o deck");
    o(rem, rc.ellipse(976, 318, 16, 24, { ...pen, strokeWidth: 1.6, fill: "#ddd", fillStyle: "solid", seed: 96 }), "o deck");

    // the lines of the chat lanes lead out from under the two computers: drawn last, the computers over the paths
    svg.append(me, rem);
}
