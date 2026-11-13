// The plugin of esbuild that gives `import code from "session-worker"` ( src/desk.ts ): the text of the worker of the session
// ( src/worker/session.ts ) bundled apart - the page starts it from a Blob URL, as a page opened from disk cannot start a worker from a
// file. Used by build.mjs here and by the build of a project that imports src/desk.ts ( DeFreez ): the paths are of this folder, the
// esbuild is the importer's, the target and the minifying are those of the build it is in. Its sources are watched with the page's.

import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));

export function sessionWorker() {
    return {
        name: "session-worker",
        setup(build) {
            const { target = "es2021", minify = false } = build.initialOptions;
            build.onResolve({ filter: /^session-worker$/ }, () => ({ path: "session-worker", namespace: "session-worker" }));
            build.onLoad({ filter: /.*/, namespace: "session-worker" }, async () => {
                const result = await build.esbuild.build({
                    entryPoints: [resolve(root, "src/worker/session.ts")], absWorkingDir: root, outfile: "session.js", write: false, metafile: true,
                    bundle: true, format: "iife", target, minify, logLevel: "warning",
                });
                return { contents: result.outputFiles[0].text, loader: "text", watchFiles: Object.keys(result.metafile.inputs).map(input => resolve(root, input)) };
            });
        },
    };
}
