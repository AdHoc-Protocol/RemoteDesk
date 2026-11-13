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
    const { drawScene } = await import("data:text/javascript;base64," + Buffer.from(bundled.outputFiles[0].text).toString("base64"));
    const { default: rough } = await import("roughjs");

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
}

const watch = process.argv.includes("--watch");
const options = {
    entryPoints: ["src/app.ts"], outfile: "viewer.js", bundle: true, format: "iife", target: "es2021", logLevel: "info", plugins: [sessionWorker(), onePage()],
    define: { HAND_FONT: JSON.stringify(HAND) },
};

if (process.argv.includes("--pictures")) await pictures();
else if (watch) await (await esbuild.context(options)).watch();
else await esbuild.build(options);
