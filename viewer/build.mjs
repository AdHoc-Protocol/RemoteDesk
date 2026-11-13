// Builds viewer.js: the login form ( src/app.ts ) and the desk ( src/desk.ts ) with the worker of the session ( src/worker/session.ts )
// bundled apart and embedded in it as text by the plugin of worker-plugin.mjs. `--watch` builds again on every change of either.
//
// And RemoteDesk.html: viewer.html with viewer.js inside it - the page as one file, to download and open, nothing beside it.
// `--pictures` draws the pictures of the page for the README instead: docs/page-*.svg.

import * as esbuild from "esbuild";
import { readFileSync, writeFileSync } from "node:fs";
import { sessionWorker } from "./worker-plugin.mjs";

/** RemoteDesk.html after each build: the script in place of its link; `</script` in it escaped, or the page would end the tag there. */
const onePage = () => ({
    name: "one-page",
    setup(build) {
        build.onEnd(result => {
            if (result.errors.length !== 0) return;
            const page = readFileSync("viewer.html", "utf8"), script = readFileSync("viewer.js", "utf8").replaceAll("</script", "<\\/script");
            const link = '<script defer src="viewer.js"></script>';
            if (!page.includes(link)) throw new Error(`viewer.html has no ${link}`);
            writeFileSync("RemoteDesk.html", page.replace(link, () => `<script>\n${script}</script>`));
        });
    },
});

/** The hand of the diagram ( Caveat, SIL Open Font License ): inside the page, as data - the page needs nothing from the internet. */
const HAND = "data:font/woff2;base64," + readFileSync("node_modules/@fontsource/caveat/files/caveat-latin-600-normal.woff2").toString("base64");

// ── `--pictures`: the scene of the page in a few moments, for the README ──
//
// docs/page-*.svg: src/scene.ts draws each as the page does, in jsdom; the style of viewer.html and the hand go inside, and what the
// page types and presses is drawn filled in. Each is one SVG that moves by itself - pulses, lights, the flying paper are animations
// of CSS and of SVG, no script: GitHub shows them so.

const SVG_NS = "http://www.w3.org/2000/svg";

/** The moments: the way, the state of each part as the page sets it, and what is typed, said and pressed then. */
const PICTURES = [
    {
        file: "page-1-ready", way: 1, parts: { me: "ok", back: "ok" }, connect: true,
        back: "192.168.1.20:8765#K7QM2XHP9RTA4WZD8CNE",
    },
    {
        file: "page-3-connecting", way: 3, parts: { me: "try", back: "ok", wa: "try", wb: "try", rem: "try" },
        back: "router.example.org:41933#K7QM2XHP9RTA4WZD8CNE", mid: ["a router of yours brings the two together", ""], rem: ["reaching it…", ""],
    },
    {
        file: "page-4-waiting", way: 4, parts: { out: "ok", back: "wait", rem: "wait" }, stun: ["ok", "ok", "no", "ok"], made: true,
        back: "", mid: ["straight through both routers, no one in between", ""], rem: ["pastes your line, sends its answer", "wait"],
    },
    {
        file: "page-4-alive", way: 4, parts: { me: "ok", out: "ok", back: "ok", wa: "up", wb: "up", mid: "ok", rem: "ok" }, stun: ["ok", "ok", "no", "ok"], made: true,
        back: "rd1a.EQh6cTJmY1M2dFZ4…", mid: ["the way through both routers is found", "ok"], rem: ["let you in", "ok"],
    },
    {
        file: "page-2-broken", way: 2, parts: { me: "ok", back: "ok", wa: "up", wb: "cut", rem: "bad" }, connect: true,
        back: "203.0.113.7:8765#K7QM2XHP9RTA4WZD8CNE", mid: ["straight to the address of its router: the router lets you in", ""],
        rem: ["not reached: is the port forwarded on its router, and its address public?", "bad"],
    },
];

async function pictures() {
    const { JSDOM } = await import("jsdom");
    const document = new JSDOM("<!doctype html><html><body></body></html>").window.document;
    globalThis.document = document;
    const bundled = await esbuild.build({ entryPoints: ["src/scene.ts"], bundle: true, format: "esm", platform: "browser", write: false });
    const drawn = await import("data:text/javascript;base64," + Buffer.from(bundled.outputFiles[0].text).toString("base64"));
    const { drawScene, rough } = drawn;

    const page = readFileSync("viewer.html", "utf8");
    const between = (from, to) => { const a = page.indexOf(from), b = page.indexOf(to, a); if (a < 0 || b < 0) throw new Error(`viewer.html has no ${from}`); return page.slice(a, b); };
    const style = `@font-face { font-family: "RD Hand"; src: url(${HAND}) format("woff2"); }\n`
        + between("        :root {", "        * { box-sizing")
        + between("        /* the diagram: a scene", "        /* what happens now;")
        + "        #map { margin: 0; border: 0; background: none; font: 14px system-ui, sans-serif; }\n";
    const filters = between('<filter id="rough"', "</svg>");

    const el = (tag, attrs, parent, text) => {
        const e = document.createElementNS(SVG_NS, tag);
        for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
        if (text !== undefined) e.textContent = text;
        parent.append(e);
        return e;
    };
    const PEN = { roughness: 1.3, bowing: 1.1, strokeWidth: 2.2, stroke: "#333" };
    const COLOR = { ok: "var(--ok)", bad: "var(--bad)", wait: "var(--warn)", try: "var(--accent)", "": "var(--ink)" };

    for (const p of PICTURES) {
        const svg = el("svg", { xmlns: SVG_NS, id: "map", viewBox: "0 0 1000 680", width: 1000, height: 680, "data-way": p.way }, document.body);
        for (const [part, state] of Object.entries(p.parts)) svg.setAttribute("data-" + part, state);
        el("title", {}, svg, "RemoteDesk: the page");
        el("style", {}, svg, style);
        const defs = el("defs", {}, svg);
        defs.innerHTML = filters + '<pattern id="dots" width="20" height="20" patternUnits="userSpaceOnUse"><circle cx="1.5" cy="1.5" r="1.1" style="fill: var(--dot)"/></pattern>';
        el("rect", { x: 1, y: 1, width: 998, height: 678, rx: 14, style: "fill: var(--paper); stroke: var(--line)" }, svg);
        el("rect", { x: 1, y: 1, width: 998, height: 678, rx: 14, fill: "url(#dots)" }, svg);
        const scene = el("svg", { id: "scene", width: 1000, height: 680 }, svg);
        drawScene(scene, p.way);
        (p.stun ?? []).forEach((s, i) => scene.querySelector(`#stun-${i}`)?.setAttribute("data-s", s));
        const rc = rough.svg(scene);

        const text = (parent, x, y, t, attrs) => el("text", { x, y, ...attrs }, parent, t);
        const field = (parent, x, y, w, value, placeholder) => {
            el("rect", { x, y, width: w, height: 32, rx: 6, style: "fill: var(--card); stroke: var(--line)" }, parent);
            text(parent, x + 10, y + 21, value || placeholder, { style: `font: 14px system-ui, sans-serif; fill: var(${value ? "--fg" : "--muted"})` });
        };
        const button = (parent, x, y, w, label, second, off) => {
            el("rect", { x, y, width: w, height: 34, rx: 6, style: second ? "fill: none; stroke: var(--accent)" : `fill: var(--accent); opacity: ${off ? .4 : 1}` }, parent);
            text(parent, x + w / 2, y + 22, label, { "text-anchor": "middle", style: `font: 14px system-ui, sans-serif; fill: ${second ? "var(--accent)" : "#fff"}` });
        };
        const tag = (x, y, w, h, state, n, head) => {
            const g = el("g", { style: `--pen: ${COLOR[state ?? ""]}` }, scene);
            el("rect", { x, y, width: w, height: h, rx: 14, style: "fill: var(--paper)" }, g);
            const box = rc.rectangle(x, y, w, h, { ...PEN, seed: 100 + y });
            box.classList.add("o");
            g.append(box);
            el("circle", { cx: x + 26, cy: y + 24, r: 12, style: "fill: var(--pen)" }, g);
            text(g, x + 26, y + 29, n, { "text-anchor": "middle", style: "font: 700 14px system-ui, sans-serif; fill: var(--paper)" });
            text(g, x + 46, y + 31, head, { class: "hand", style: "font-size: 22px; font-weight: 700; fill: var(--pen)" });
            return g;
        };
        /** Words written by hand under a part, broken into lines that fit `w`. */
        const say = (cx, y, w, [words, kind]) => {
            const lines = [], most = Math.floor(w / 8.4);
            for (const word of words.split(" ")) {
                const last = lines.length - 1;
                if (last >= 0 && (lines[last] + " " + word).length <= most) lines[last] += " " + word;
                else lines.push(word);
            }
            lines.forEach((line, i) => text(scene, cx, y + 20 + i * 22, line, { class: "hand", "text-anchor": "middle", style: `font-size: 21px; fill: ${COLOR[kind]}` }));
        };

        // this computer: the name, Connect
        text(scene, 75, 338, "Your name", { class: "hand", style: "font-size: 20px; fill: var(--ink)" });
        field(scene, 75, 346, 155, "Anna", "");
        button(scene, 75, 388, 155, "Connect", false, !p.connect);
        if (p.mid) say(500, p.way === 1 ? 360 : 333, 320, p.mid);
        if (p.rem) say(827, 337, 215, p.rem);

        // by chat or mail: the line there ( way 4 ), the line or the answer back
        if (p.way === 4) {
            const out = tag(240, 388, 520, 82, p.parts.out, "1", "Your line flies there, by chat or mail");
            button(out, 254, 424, 150, p.made ? "Make a new line" : "Make the line", p.made, false);
            if (p.made) text(out, 418, 446, "✓ It is in your clipboard: send it.", { style: "font: 13px system-ui, sans-serif; fill: var(--ok)" });
        }
        const y = p.way === 4 ? 554 : 503;
        const back = tag(240, y, 520, 86, p.parts.back, p.way === 4 ? "2" : "1", p.way === 4 ? "Its answer comes back: paste it here" : "Its line comes to you: paste it here");
        field(back, 254, y + 44, 492, p.back, "rd1a.EQh6cTJmY1M2dFZ4b0pXa3RnU2xh…");

        writeFileSync(`../docs/${p.file}.svg`, svg.outerHTML + "\n");
        svg.remove();
        console.log(`docs/${p.file}.svg`);
    }

    // ── the other pictures of the README, in the same hand: what it is, what happens at Connect, several viewers ──

    /** A picture `height` high, of the part of the scene from `top` down; `more` - its own style. */
    const frame = (title, top, height, attrs, more = "") => {
        const svg = el("svg", { xmlns: SVG_NS, id: "map", viewBox: `0 ${top} 1000 ${height}`, width: 1000, height, ...attrs }, document.body);
        el("title", {}, svg, title);
        el("style", {}, svg, style + more);
        const defs = el("defs", {}, svg);
        defs.innerHTML = filters + '<pattern id="dots" width="20" height="20" patternUnits="userSpaceOnUse"><circle cx="1.5" cy="1.5" r="1.1" style="fill: var(--dot)"/></pattern>';
        for (const fill of ["fill: var(--paper); stroke: var(--line)", "fill: url(#dots)"])
            el("rect", { x: 1, y: top + 1, width: 998, height: height - 2, rx: 14, style: fill }, svg);
        const scene = el("svg", { id: "scene", width: 1000, height: 680, viewBox: "0 0 1000 680" }, svg);
        return { svg, scene, rc: drawn.rough.svg(scene) };
    };
    const save = (file, svg) => {
        writeFileSync(`../docs/${file}.svg`, svg.outerHTML + "\n");
        svg.remove();
        console.log(`docs/${file}.svg`);
    };
    const hand = (parent, x, y, words, size, fill, anchor = "middle") =>
        el("text", { x, y, class: "hand", "text-anchor": anchor, style: `font-size: ${size}px; fill: ${fill}` }, parent, words);
    /** Words broken into lines of `most` letters, from `y` down. */
    const lines = (words, most) => {
        const out = [];
        for (const word of words.split(" ")) {
            const last = out.length - 1;
            if (last >= 0 && (out[last] + " " + word).length <= most) out[last] += " " + word;
            else out.push(word);
        }
        return out;
    };

    // what it is: your laptop and the remote computer, alive between them through the internet; what goes each way
    {
        const { svg, scene } = frame("RemoteDesk: you see and control the remote computer from a browser", 96, 316,
            { "data-way": 2, "data-me": "ok", "data-wa": "up", "data-wb": "up", "data-mid": "ok", "data-rem": "ok" });
        drawScene(scene, 2);
        scene.querySelector("#chat").remove();
        scene.querySelector("#pressed").textContent = "Pressed: 2";
        hand(scene, 500, 352, "← the screen, the sound, the clipboard, files", 22, "var(--ok)");
        hand(scene, 500, 384, "your keys, mouse, touch, the microphone →", 22, "var(--accent)");
        save("overview", svg);
    }

    // what happens at Connect: four steps, lit one after another
    {
        const STEPS = [
            ["Encrypted", "Noise from here on: nobody between reads or changes anything"],
            ["Its key", "The computer proves its key. The page keeps it the first time; later it must be the same"],
            ["The token", "From the line, with your name. 5 wrong in a row: nobody gets in for a minute"],
            ["In", "The screen comes; the remote computer prints who came in, from where"],
        ];
        const more = STEPS.map((_, i) => `        #step-${i} .o path { animation: lit 8s ${i * 2}s infinite; } #step-${i} .num { animation: lit-num 8s ${i * 2}s infinite; }\n`).join("")
            + "        #steps .o path { stroke: var(--ink); } #steps .num { fill: var(--ink); }\n"
            + "        @keyframes lit { 0%, 24% { stroke: var(--ok); } 26%, 100% { stroke: var(--ink); } }\n"
            + "        @keyframes lit-num { 0%, 24% { fill: var(--ok); } 26%, 100% { fill: var(--ink); } }\n"
            + "        #steps .num { stroke: none !important; }\n"
            + "        #steps .dot { opacity: 1; fill: var(--ok); }\n";
        const { svg, scene, rc } = frame("RemoteDesk: what happens when you press Connect", 0, 360, {}, more);
        const g = el("g", { id: "steps" }, scene);
        const track = "M 130 110 L 870 110";
        el("path", { id: "track", d: track, fill: "none" }, g);
        drawn.outline(g, rc.path(track, { ...PEN, strokeWidth: 2, strokeLineDash: [8, 8], seed: 200 }));
        drawn.pulses(g, "track", 2, 4, false);
        STEPS.forEach(([title, words], i) => {
            const x = 28 + i * 240, y = 24, step = el("g", { id: `step-${i}` }, g);
            el("rect", { x, y, width: 220, height: 250, rx: 14, style: "fill: var(--paper)" }, step);
            drawn.outline(step, rc.rectangle(x, y, 220, 250, { ...PEN, seed: 210 + i }));
            el("circle", { cx: x + 24, cy: y + 24, r: 13, class: "num" }, step);
            hand(step, x + 24, y + 30, String(i + 1), 18, "var(--paper)");
            const cx = x + 110, cy = y + 74, o = (shape) => drawn.outline(step, shape);
            if (i === 0) { // a lock
                o(rc.rectangle(cx - 26, cy - 6, 52, 40, { ...PEN, fill: "#e8e8e8", fillStyle: "hachure", hachureGap: 6, seed: 220 }));
                o(rc.arc(cx, cy - 6, 34, 44, Math.PI, Math.PI * 2, false, { ...PEN, seed: 221 }));
            } else if (i === 1) { // a key
                o(rc.circle(cx - 24, cy + 10, 30, { ...PEN, seed: 222 }));
                o(rc.linearPath([[cx - 9, cy + 10], [cx + 36, cy + 10], [cx + 36, cy + 22], [cx + 26, cy + 10], [cx + 22, cy + 20]], { ...PEN, seed: 223 }));
            } else if (i === 2) { // a ticket, the token on it
                o(rc.rectangle(cx - 44, cy - 10, 88, 46, { ...PEN, seed: 224 }));
                hand(step, cx, cy + 20, "#K7QM…", 20, "var(--accent)");
            } else { // a screen, windows on it
                o(rc.rectangle(cx - 40, cy - 14, 80, 54, { ...PEN, seed: 225 }));
                o(rc.rectangle(cx - 32, cy - 6, 34, 22, { ...PEN, strokeWidth: 1.4, fill: "#9cc4f0", fillStyle: "hachure", hachureGap: 5, seed: 226 }));
                o(rc.rectangle(cx + 6, cy + 4, 26, 26, { ...PEN, strokeWidth: 1.4, seed: 227 }));
            }
            hand(step, cx, y + 146, title, 26, "var(--ink)");
            lines(words, 26).forEach((line, k) => hand(step, cx, y + 170 + k * 18, line, 17, "var(--muted)"));
        });
        lines("The first time, compare the key the page shows with the line key that rdp-host printed: the same - it is your computer", 90)
            .forEach((line, k) => hand(scene, 500, 306 + k * 24, line, 21, "var(--ink)"));
        save("connect", svg);
    }

    // several viewers: all see through the internet, one controls - its pulses both ways, the others' toward them only
    {
        const VIEWERS = [["Anna", "controls", "var(--ok)"], ["Boris", "watches", "var(--accent)"], ["Vera", "watches another display", "var(--accent)"]];
        const more = VIEWERS.map(([, , color], i) => `        #v-${i} .wire { stroke: ${color}; } #v-${i} .dot.back${i === 0 ? ", #v-0 .dot.fwd" : ""} { opacity: 1; fill: ${color}; }\n`).join("")
            + "        #viewers .wire { fill: none; stroke-width: 3; opacity: .8; }\n";
        const { svg, scene, rc } = frame("RemoteDesk: several viewers, all see, one controls", 20, 400, { "data-rem": "ok" }, more);
        const g = el("g", { id: "viewers" }, scene);
        drawn.outline(g, rc.path(drawn.cloudPath(480, 210, 230, 160), { ...PEN, strokeWidth: 2.4, fill: "#9cc4f0", fillStyle: "hachure", hachureGap: 9, hachureAngle: -30, seed: 300 }), "o cloud");
        hand(g, 480, 268, "Internet", 30, "var(--ink)"); // under the lines that cross the cloud
        VIEWERS.forEach(([name, does], i) => {
            const top = 40 + i * 120, v = el("g", { id: `v-${i}` }, g);
            const d = `M 190 ${top + 52} C 300 ${top + 52} 360 ${206 + (i - 1) * 14} 470 ${206 + (i - 1) * 14} L 560 ${206 + (i - 1) * 14} C 650 ${206 + (i - 1) * 14} 700 ${214 + (i - 1) * 8} 748 ${214 + (i - 1) * 8}`;
            el("path", { id: `v-path-${i}`, d, class: "wire" }, v);
            drawn.pulses(v, `v-path-${i}`, 2, 2.4, true);
            const small = el("g", { transform: `translate(${30 - 24 * 0.55} ${top - 146 * 0.55}) scale(0.55)` }, v);
            drawn.laptop(rc, small, name);
            hand(v, 24, top + 112, `${name} · ${does}`, 20, VIEWERS[i][2], "start");
        });
        const rem = el("g", { id: "n-rem" }, scene);
        hand(rem, 855, 126, "Remote computer", 30, "var(--ink)");
        drawn.monitor(rc, rem, ["> rdp-host", "in: Anna, Boris, Vera", "Anna controls", "3 watching"]);
        hand(scene, 860, 372, "one picture, sent to each;", 20, "var(--muted)");
        hand(scene, 860, 396, "Take control moves the keys", 20, "var(--muted)");
        save("viewers", svg);
    }
}

const watch = process.argv.includes("--watch");
const options = {
    entryPoints: ["src/app.ts"], outfile: "viewer.js", bundle: true, format: "iife", target: "es2021", logLevel: "info", plugins: [sessionWorker(), onePage()],
    define: { HAND_FONT: JSON.stringify(HAND) },
};

if (process.argv.includes("--pictures")) await pictures();
else if (watch) await (await esbuild.context(options)).watch();
else await esbuild.build(options);
