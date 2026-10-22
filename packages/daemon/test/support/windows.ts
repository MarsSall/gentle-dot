import { describe, expect, it } from "vitest";

export const WINDOWS_APP_CHANNEL =
	"Windows admits only the native production app channel (daemon.ts startDaemon); the protocol is covered on Linux and macOS";

export const POSIX_FILE_MODE =
	"Windows has no POSIX permission bits; private files rely on the user profile's ACL. Linux and macOS cover these mode assertions";

const windows = process.platform === "win32";

/** Keep the test and its stat/lstat running; only POSIX permission bits are platform-specific. */
export function expectPosixMode(actualMode: number, expectedMode: number): void {
	if (!windows) expect(actualMode & 0o777, POSIX_FILE_MODE).toBe(expectedMode);
}

/** Only daemon tests requiring a generic Duplex app channel lack Windows coverage. */
export const describeWithAppChannel: ReturnType<typeof describe.skipIf> = describe.skipIf(windows);
export const itWithAppChannel: ReturnType<typeof it.skipIf> = it.skipIf(windows);

if (windows) {
	console.info(`Windows-only test skip: ${WINDOWS_APP_CHANNEL}`);
}
