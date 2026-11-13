// Files between the page and the machine, the page's half: the worker of the session moves them ( ./worker/files.ts ), the page
// shows how it goes, offers to cancel and saves a file the machine sent. Files are picked, or dropped on the screen, in ./desk.ts.

export interface Files {
    /** How the transfer goes; `busy` - one goes on: it can be cancelled. */
    show(text: string, busy: boolean): void;
    /** Saves `blob` as `name`: the browser puts it in the downloads of this device. */
    save(blob: Blob, name: string): void;
    stop(): void;
}

/**
 * `status` shows how the transfer goes, `cancelButton` is shown while it goes.
 */
export function start(status: HTMLElement, cancelButton: HTMLElement): Files {
    return {
        show(text, busy) {
            status.textContent = text;
            cancelButton.hidden = !busy;
        },
        save(blob, name) {
            const url = URL.createObjectURL(blob);
            const link = document.createElement("a");
            link.href = url;
            link.download = name;
            link.click();
            setTimeout(() => URL.revokeObjectURL(url), 60_000);
        },
        stop() {
            status.textContent = "";
            cancelButton.hidden = true;
        },
    };
}
