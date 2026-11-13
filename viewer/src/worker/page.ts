// The way from the worker of the session to the page: the messages of ../messages.ts. The worker is typed with the DOM of the page
// ( one tsconfig for both ), so its global scope is described here as much as it is used.

import type { ToMain, ToWorker } from "../messages";

const scope = self as unknown as {
    postMessage(message: ToMain, options?: StructuredSerializeOptions): void;
    onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
};

/** Posts `message` to the page; what `transfer` holds moves there, not copied. */
export function toPage(message: ToMain, transfer?: Transferable[]): void {
    scope.postMessage(message, transfer === undefined ? undefined : { transfer });
}

/** `handle` takes the messages of the page. */
export function listen(handle: (message: ToWorker) => void): void {
    scope.onmessage = e => handle(e.data);
}
