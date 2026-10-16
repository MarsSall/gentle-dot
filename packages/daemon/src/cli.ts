#!/usr/bin/env node
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import type { Duplex } from "node:stream";
import { inheritedAppSocket } from "./app-channel.ts";
import { loadConfig } from "./config.ts";
import { APPROVAL_GUARD, bundledMcpCli } from "./connectors.ts";
import { startDaemon, startupMessage } from "./daemon.ts";
import { resolveEngramBin, runtimeDir } from "./runtime.ts";
import { IDENTITY_SOURCE } from "./white-label.ts";
import { finishWindowsShutdown, startWithWindowsAppChannel } from "./windows-stop.ts";
import type { WindowsAppChannel } from "./windows-app-channel.ts";

const log = (line: string) => process.stderr.write(`${new Date().toISOString()} ${line}\n`);

/**
 * `--self-check`: resolves the files the daemon runs on, without starting anything or
 * using the network, prints them as JSON, and exits 1 when one is missing.
 */
function selfCheck(): void {
	const config = loadConfig();
	const runtime = runtimeDir();
	const report = {
		runtime,
		uiDir: config.uiDir,
		gentleShell: config.agentArgs[0],
		engineCli: bundledMcpCli().args[0],
		approvalGuard: APPROVAL_GUARD,
		identity: fileURLToPath(IDENTITY_SOURCE),
		engramBin: runtime ? resolveEngramBin(process.env, runtime) : undefined,
	};
	const required = [report.gentleShell, report.engineCli, report.approvalGuard, report.identity];
	const ok = required.every((path) => path !== undefined && existsSync(path));
	process.stdout.write(`${JSON.stringify({ ok, ...report }, null, 2)}\n`);
	process.exit(ok ? 0 : 1);
}

async function main(): Promise<void> {
	const config = loadConfig();
	// The desktop app that launched the daemon hands over its private channel on fd 3 (S25.1); it is
	// opened before anything is spawned, and refused if children would inherit it.
	const appChannel = process.platform === "win32" ? undefined : inheritedAppSocket({ log });
	const start = (channel: Duplex | undefined = appChannel) => startDaemon({
		port: config.port,
		host: config.host,
		dataDir: config.dataDir,
		workspace: config.workspace,
		...(config.preferredFolder ? { preferredFolder: config.preferredFolder } : {}),
		uiDir: config.uiDir,
		agentCommand: config.agentCommand,
		agentArgs: config.agentArgs,
		...(config.agentHome ? { agentHome: config.agentHome } : {}),
		allowedOrigins: config.allowedOrigins,
		...(channel ? { appChannel: channel } : {}),
		log,
	});
	// Windows requires the L98 private exact-launch record. No implicit unmanaged bypass.
	// Construction precedes startDaemon and therefore all descendant startup.
	const windows = process.platform === "win32"
		? await startWithWindowsAppChannel<Awaited<ReturnType<typeof start>>, WindowsAppChannel>(join(config.dataDir, "daemon.pid"), start)
		: undefined;
	const daemon = windows?.daemon ?? await start();
	process.stdout.write(`${startupMessage(daemon.url, config.dataDir, process.stdout.isTTY === true)}\n`);

	if (windows) {
		const shutdown = () => { void windows.close(); };
		process.on("SIGINT", shutdown);
		process.on("SIGTERM", shutdown);
		// Only successful daemon.close AND native drain may report success. Never use
		// daemon.closed directly here: it also settles when daemon.close rejects.
		// On failure, retain the owner and set exitCode without abandoning native
		// ownership through process.exit. No descendant completion is claimed.
		void finishWindowsShutdown(windows, process, log);
		return;
	}

	let closing = false;
	const shutdown = (signal: string) => {
		if (closing) return;
		closing = true;
		log(`received ${signal}, shutting down`);
		daemon.close().then(
			() => process.exit(0),
			() => process.exit(1),
		);
	};
	process.on("SIGINT", () => shutdown("SIGINT"));
	process.on("SIGTERM", () => shutdown("SIGTERM"));
	// A daemon the app launched stops itself when the app's channel closes (S35.2); so does the process.
	void daemon.closed.then(() => process.exit(0));
}

if (process.argv.includes("--self-check")) selfCheck();

main().catch((error: Error) => {
	log(`failed to start: ${error.message}`);
	if (process.platform === "win32") process.exitCode = 1;
	else process.exit(1);
});
