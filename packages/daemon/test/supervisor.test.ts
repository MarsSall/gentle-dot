import * as childProcess from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentSupervisor } from "../src/supervisor.ts";
import { FAKE_AGENT, fakeSupervisor, tempDir, waitFor } from "./helpers.ts";

vi.mock("node:child_process", { spy: true });

const running: AgentSupervisor[] = [];
afterEach(async () => {
	await Promise.all(running.splice(0).map((s) => s.stop()));
});

function track<T extends { supervisor: AgentSupervisor }>(value: T): T {
	running.push(value.supervisor);
	return value;
}

const STUBBORN_AGENT = `
process.on("SIGTERM", () => {});
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
	buffer += chunk;
	for (;;) {
		const newline = buffer.indexOf("\\n");
		if (newline < 0) break;
		const line = buffer.slice(0, newline);
		buffer = buffer.slice(newline + 1);
		if (!line) continue;
		const record = JSON.parse(line);
		if (record.type === "get_state") {
			process.stdout.write(JSON.stringify({
				type: "response", id: record.id, command: "get_state", success: true,
				data: { sessionId: "stubborn" }
			}) + "\\n");
		}
	}
});
process.stdin.on("end", () => process.stdin.resume());
setInterval(() => {}, 1000);
`;

describe("AgentSupervisor", () => {
	it("enforces packaged Windows setup opt-outs after prepareSpawn on every spawn without launching the engine", async () => {
		const runtime = join(process.cwd(), "packaged-runtime");
		const script = join(runtime, "daemon", "node_modules", "gentle-pi", "bin", "gentle-shell.mjs");
		const spawn = vi.spyOn(childProcess, "spawn").mockImplementation(() => {
			throw new Error("spawn intercepted");
		});
		try {
			const { supervisor } = fakeSupervisor({
				platform: "win32",
				args: [script, "--home", "/agent"],
				env: { GENTLE_DOT_RUNTIME: runtime, GENTLE_SHELL_NO_AUTO_SETUP: "0" },
				prepareSpawn: () => ({ GENTLE_PI_SKIP_GENTLE_AI_INSTALL: "0", PREPARED: "yes" }),
			});
			for (let attempt = 0; attempt < 2; attempt++) {
				await expect(supervisor.start()).rejects.toThrow("spawn intercepted");
				const options = spawn.mock.calls[attempt]?.[2] as childProcess.SpawnOptions;
				expect(options.env).toMatchObject({
					GENTLE_SHELL_NO_AUTO_SETUP: "1",
					GENTLE_PI_SKIP_GENTLE_AI_INSTALL: "1",
					PREPARED: "yes",
				});
			}
		} finally {
			spawn.mockRestore();
		}
	});

	it("spawns the agent in RPC mode with the session dir and extra args", async () => {
		const argsFile = join(tempDir(), "argv.json");
		const { supervisor, dataDir } = track(
			fakeSupervisor({ env: { ...process.env, FAKE_AGENT_ARGS_FILE: argsFile }, extraArgs: ["--x", "1"] }),
		);
		await supervisor.start();
		const argv = JSON.parse(readFileSync(argsFile, "utf8")) as string[];
		expect(argv).toEqual(["--mode", "rpc", "--session-dir", join(dataDir, "sessions"), "--x", "1"]);
		expect(supervisor.state).toBe("ready");
	});

	it("starts the agent as the configured user, and as the daemon's own user without one (S25.8)", async () => {
		const own = { uid: process.getuid?.() ?? 0, gid: process.getgid?.() ?? 0 };
		const processFile = join(tempDir(), "process.json");
		const env = { ...process.env, FAKE_AGENT_PROCESS_FILE: processFile };
		// Without a user, the agent runs as the daemon does today.
		const plain = track(fakeSupervisor({ env }));
		await plain.supervisor.start();
		expect(JSON.parse(readFileSync(processFile, "utf8"))).toMatchObject(own);
		await plain.supervisor.stop();
		const as = track(fakeSupervisor({ env, user: own }));
		await as.supervisor.start();
		expect(JSON.parse(readFileSync(processFile, "utf8"))).toMatchObject(own);
		expect(as.supervisor.state).toBe("ready");
	});

	it.skipIf((process.getuid?.() ?? 0) === 0)(
		"applies the user to the spawn: one it cannot take is refused",
		async () => {
			const own = { uid: process.getuid?.() ?? 0, gid: process.getgid?.() ?? 0 };
			const { supervisor } = track(
				fakeSupervisor({ user: { uid: own.uid + 1, gid: own.gid }, startTimeoutMs: 2000 }),
			);
			await expect(supervisor.start()).rejects.toThrow();
			expect(supervisor.state).not.toBe("ready");
		},
	);

	it("correlates responses by id across concurrent commands", async () => {
		const { supervisor } = track(fakeSupervisor());
		await supervisor.start();
		const [state, commands] = await Promise.all([
			supervisor.request({ type: "get_state" }),
			supervisor.request({ type: "get_commands" }),
		]);
		expect(state.command).toBe("get_state");
		expect(commands.command).toBe("get_commands");
	});

	it("rejects a failed command with the agent's error", async () => {
		const { supervisor } = track(fakeSupervisor());
		await supervisor.start();
		await expect(supervisor.request({ type: "switch_session", sessionPath: "/nope.jsonl" })).rejects.toThrow(
			"Session not found",
		);
	});

	it("forwards session events and tracks busy until agent_settled", async () => {
		const { supervisor, events } = track(fakeSupervisor());
		await supervisor.start();
		await supervisor.request({ type: "prompt", message: "hello" });
		await waitFor(() => events.some((e) => e.type === "agent_settled"));
		expect(events.map((e) => e.type)).toContain("message_update");
		expect(supervisor.busy).toBe(false);
	});

	it("is not left busy when the run settles before the prompt response is handled", async () => {
		const { supervisor, events } = track(fakeSupervisor());
		await supervisor.start();
		await supervisor.request({ type: "prompt", message: "burst" });
		expect(events.some((e) => e.type === "agent_settled")).toBe(true);
		expect(supervisor.busy).toBe(false);
	});

	it("respawns after a crash, reopens the same conversation, and reports the interruption", async () => {
		const { supervisor, events } = track(fakeSupervisor());
		await supervisor.start();
		await supervisor.request({ type: "prompt", message: "first" });
		await waitFor(() => events.some((e) => e.type === "agent_settled"));
		const sessionBefore = supervisor.sessionFile;
		const pidBefore = supervisor.pid;

		await supervisor.request({ type: "prompt", message: "crash" });
		await waitFor(() => events.some((e) => e.type === "interrupted"));

		expect(supervisor.state).toBe("ready");
		expect(supervisor.pid).not.toBe(pidBefore);
		expect(supervisor.sessionFile).toBe(sessionBefore);
		const states = events.flatMap((e) => (e.type === "supervisor_state" && "state" in e ? [e.state] : []));
		expect(states).toContain("restarting");
		const history = await supervisor.request({ type: "get_messages" });
		const texts = (history.data as { messages: { content: unknown }[] }).messages.map((m) => m.content);
		expect(texts[0]).toBe("first");
	});

	it("does not report an interruption when the agent dies while idle", async () => {
		const { supervisor, events } = track(fakeSupervisor());
		await supervisor.start();
		process.kill(supervisor.pid ?? 0, "SIGKILL");
		await waitFor(
			() =>
				events.filter((e) => e.type === "supervisor_state" && "state" in e && e.state === "ready").length >=
				2,
		);
		expect(events.some((e) => e.type === "interrupted")).toBe(false);
	});

	it("rejects pending commands when the agent exits", async () => {
		const { supervisor } = track(fakeSupervisor());
		await supervisor.start();
		const pending = supervisor.request({ type: "noreply" });
		process.kill(supervisor.pid ?? 0, "SIGKILL");
		await expect(pending).rejects.toThrow(/agent exited/i);
	});

	it("reopens the last conversation after the daemon itself restarts", async () => {
		const dataDir = tempDir();
		const first = track(fakeSupervisor({ dataDir }));
		await first.supervisor.start();
		await first.supervisor.request({ type: "prompt", message: "remember me" });
		await waitFor(() => first.events.some((e) => e.type === "agent_settled"));
		await waitFor(() => first.supervisor.sessionFile && first.supervisor.sessionFile);
		const session = first.supervisor.sessionFile;
		await first.supervisor.stop();

		const second = track(fakeSupervisor({ dataDir }));
		await second.supervisor.start();
		expect(second.supervisor.sessionFile).toBe(session);
	});

	it("restarts on request, keeping the conversation and without an interruption", async () => {
		const { supervisor, events } = track(fakeSupervisor());
		await supervisor.start();
		await supervisor.request({ type: "prompt", message: "keep me" });
		await waitFor(() => events.some((e) => e.type === "agent_settled"));
		const session = supervisor.sessionFile;
		const pid = supervisor.pid;
		await supervisor.restart();
		expect(supervisor.state).toBe("ready");
		expect(supervisor.pid).not.toBe(pid);
		expect(supervisor.sessionFile).toBe(session);
		expect(events.some((e) => e.type === "interrupted")).toBe(false);
	});

	it.each(["stop during restart", "concurrent restarts"])("serializes lifecycle: %s", async (scenario) => {
		const scratch = tempDir();
		const pidsFile = join(scratch, "children.txt");
		const recorder = join(scratch, "record.mjs");
		writeFileSync(
			recorder,
			`import {appendFileSync} from 'node:fs'; appendFileSync(${JSON.stringify(pidsFile)}, process.pid + '\\n');`,
		);
		let spawns = 0;
		const { supervisor } = track(
			fakeSupervisor({
				args: ["--import", recorder, FAKE_AGENT],
				prepareSpawn: () => {
					spawns++;
					return {};
				},
			}),
		);
		try {
			await supervisor.start();
			const session = supervisor.sessionFile;
			if (scenario === "stop during restart") {
				await Promise.all([supervisor.restart(), supervisor.stop()]);
				expect(supervisor.state).toBe("stopped");
				expect(supervisor.pid).toBeUndefined();
				expect(spawns).toBe(1);
				await expect(supervisor.request({ type: "get_state" })).rejects.toThrow(/not ready/i);
			} else {
				const results = await Promise.allSettled([supervisor.restart(), supervisor.restart()]);
				expect(results.map((result) => result.status)).toEqual(["fulfilled", "fulfilled"]);
				expect(spawns).toBe(2);
				expect(supervisor.state).toBe("ready");
				expect(supervisor.sessionFile).toBe(session);
			}
		} finally {
			await supervisor.stop();
			// Clean up only child PIDs recorded by this fixture, including a duplicate on RED.
			if (existsSync(pidsFile))
				for (const text of readFileSync(pidsFile, "utf8").trim().split("\n")) {
					const pid = Number(text);
					if (!Number.isSafeInteger(pid) || pid <= 0) continue;
					try {
						process.kill(pid, "SIGKILL");
					} catch {}
				}
		}
	});

	it("a stop during a restart starts nothing again (S35.2: the app quitting does both)", async () => {
		const { supervisor } = track(fakeSupervisor());
		await supervisor.start();
		const restarting = supervisor.restart();
		await supervisor.stop();
		await restarting;
		await new Promise((resolve) => setTimeout(resolve, 200));
		expect(supervisor.state).toBe("stopped");
		expect(supervisor.pid).toBeUndefined();
	});

	it("stops cleanly and reports the stopped state", async () => {
		const { supervisor } = track(fakeSupervisor());
		await supervisor.start();
		await supervisor.stop();
		expect(supervisor.state).toBe("stopped");
		await expect(supervisor.request({ type: "get_state" })).rejects.toThrow(/not ready/i);
	});

	it("closes a cooperative agent within the production grace without escalation", async () => {
		const escalations: boolean[] = [];
		const { supervisor } = track(
			fakeSupervisor({
				// Keep the production grace: OS scheduling under suite load is not a 10ms contract.
				terminateProcessTree: async (_child, force) => {
					escalations.push(force);
					return false;
				},
			}),
		);
		await supervisor.start();
		await supervisor.stop();
		expect(escalations).toEqual([]);
		expect(supervisor.state).toBe("stopped");
		expect(supervisor.pid).toBeUndefined();
	});

	it.each([
		["win32", 12000],
		["linux", 2000],
		["darwin", 2000],
	] as const)(
		"coordinates default %s escalation windows without changing Unix timing",
		async (platform, windowMs) => {
			const escalations: boolean[] = [];
			const { supervisor } = track(
				fakeSupervisor({
					platform,
					command: process.execPath,
					args: ["-e", STUBBORN_AGENT, "--"],
					stopTimeoutMs: 1,
					terminateProcessTree: async (child, force) => {
						escalations.push(force);
						if (force) child.kill("SIGKILL");
						return true;
					},
				}),
			);
			await supervisor.start();
			vi.useFakeTimers();
			const stopped = supervisor.stop();
			void stopped.catch(() => {});
			try {
				await vi.advanceTimersByTimeAsync(1);
				expect(escalations).toEqual([false]);
				await vi.advanceTimersByTimeAsync(windowMs - 1);
				expect(escalations).toEqual([false]);
				await vi.advanceTimersByTimeAsync(1);
				expect(escalations).toEqual([false, true]);
				await stopped;
				expect(supervisor.state).toBe("stopped");
				expect(supervisor.pid).toBeUndefined();
			} finally {
				vi.useRealTimers();
				const pid = supervisor.pid;
				if (pid !== undefined) {
					if (pid <= 0) throw new Error("Missing positive owned fixture PID");
					process.kill(pid, "SIGKILL");
					await waitFor(() => supervisor.state === "stopped");
				}
			}
		},
	);

	it("keeps both Windows default stages bounded without rejecting before taskkill can finish", async () => {
		const escalations: boolean[] = [];
		const { supervisor } = track(
			fakeSupervisor({
				platform: "win32",
				command: process.execPath,
				args: ["-e", STUBBORN_AGENT, "--"],
				stopTimeoutMs: 1,
				terminateProcessTree: async (_child, force) => {
					escalations.push(force);
					return false;
				},
			}),
		);
		await supervisor.start();
		const pid = supervisor.pid;
		const sessionId = supervisor.sessionId;
		vi.useFakeTimers();
		let failure: Error | undefined;
		const stopped = supervisor.stop().catch((error) => {
			failure = error;
		});
		try {
			await vi.advanceTimersByTimeAsync(24000);
			expect(failure).toBeUndefined();
			expect(escalations).toEqual([false, true]);
			await vi.advanceTimersByTimeAsync(1);
			await stopped;
			expect(failure?.message).toBe(`Failed to stop agent process ${pid} after bounded termination`);
			expect(supervisor.pid).toBe(pid);
			expect(supervisor.sessionId).toBe(sessionId);
		} finally {
			vi.useRealTimers();
			if (!pid || pid <= 0 || supervisor.pid !== pid)
				throw new Error("Missing current positive owned fixture PID");
			process.kill(pid, "SIGKILL");
			await waitFor(() => supervisor.state === "stopped");
		}
	});

	it("rejects stop actionably when bounded soft and hard termination do not close the child", async () => {
		const escalations: boolean[] = [];
		const { supervisor } = fakeSupervisor({
			command: process.execPath,
			args: ["-e", STUBBORN_AGENT, "--"],
			stopTimeoutMs: 10,
			hardStopTimeoutMs: 10,
			terminationFailureTimeoutMs: 10,
			terminateProcessTree: async (_child, force) => {
				escalations.push(force);
				return false;
			},
		});
		await supervisor.start();
		const pid = supervisor.pid;
		await expect(supervisor.stop()).rejects.toThrow(/failed to stop agent process/i);
		expect(escalations).toEqual([false, true]);
		expect(supervisor.pid).toBe(pid);
		if (!pid || pid <= 0) throw new Error("Missing owned test child PID");
		process.kill(pid, "SIGKILL");
		await waitFor(() => supervisor.state === "stopped");
	});

	it("rejects restart actionably instead of spawning over a child that did not terminate", async () => {
		const { supervisor } = fakeSupervisor({
			command: process.execPath,
			args: ["-e", STUBBORN_AGENT, "--"],
			stopTimeoutMs: 10,
			hardStopTimeoutMs: 10,
			terminationFailureTimeoutMs: 10,
			terminateProcessTree: async () => false,
		});
		await supervisor.start();
		const pid = supervisor.pid;
		await expect(supervisor.restart()).rejects.toThrow(/failed to stop agent process/i);
		expect(supervisor.pid).toBe(pid);
		if (!pid || pid <= 0) throw new Error("Missing owned test child PID");
		const stopped = supervisor.stop();
		process.kill(pid, "SIGKILL");
		await stopped;
		expect(supervisor.state).toBe("stopped");
	});
});
