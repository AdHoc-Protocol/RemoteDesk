// Builds viewer.js: the login form ( src/app.ts ) and the desk ( src/desk.ts ) with the worker of the session ( src/worker/session.ts )
// bundled apart and embedded in it as text by the plugin of worker-plugin.mjs. `--watch` builds again on every change of either.
//
// And RemoteDesk.html: viewer.html with viewer.js inside it - the page as one file, to download and open, nothing beside it.

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

const watch = process.argv.includes("--watch");
const options = { entryPoints: ["src/app.ts"], outfile: "viewer.js", bundle: true, format: "iife", target: "es2021", logLevel: "info", plugins: [sessionWorker(), onePage()] };

if (watch) await (await esbuild.context(options)).watch();
else await esbuild.build(options);
