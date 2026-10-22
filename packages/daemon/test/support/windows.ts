import { describe, it } from "vitest";

export const WINDOWS_APP_CHANNEL =
	"Windows admits only the native production app channel (daemon.ts startDaemon); the protocol is covered on Linux and macOS";

const windows = process.platform === "win32";

/** Only daemon tests requiring a generic Duplex app channel lack Windows coverage. */
export const describeWithAppChannel: ReturnType<typeof describe.skipIf> = describe.skipIf(windows);
export const itWithAppChannel: ReturnType<typeof it.skipIf> = it.skipIf(windows);

if (windows) {
	console.info(`Windows-only test skip: ${WINDOWS_APP_CHANNEL}`);
}
