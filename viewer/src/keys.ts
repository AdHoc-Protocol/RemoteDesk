// What the regions of Noise of protocol/gen/Session/Connection.ts take ( [Noise] of RemoteDesk.cs ): the static key of this page
// for "The keys of Noise", the check of the key the machine proved for "The key the far end proved". The page sets them.

export const noise = {
    /** The secret key of this page, X25519: `undefined` - no connection. */
    secret: (): Uint8Array | undefined => undefined,
    /** The machine dialed proved this key: `false` - the connection ends. */
    proved: (_key: Uint8Array): boolean => false,
};
