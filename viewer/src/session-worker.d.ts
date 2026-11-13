// The worker of the session ( ./worker/session.ts ) bundled apart by the plugin of ../worker-plugin.mjs and given to ./desk.ts as the
// text of its script. Another project that imports desk.ts builds with the same plugin.

declare module "session-worker" {
    const code: string;
    export default code;
}
