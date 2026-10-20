import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { act, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
	invoke: vi.fn(),
	listeners: new Map<string, (event: { payload: unknown }) => void>(),
	reducedMotion: false,
	frames: new Map<number, FrameRequestCallback>(),
	nextFrame: 0,
}));

vi.mock("@tauri-apps/api/core", () => ({
	isTauri: () => true,
	invoke: (command: string, args?: unknown) => h.invoke(command, args),
}));

vi.mock("@tauri-apps/api/event", () => ({
	listen: async (name: string, handler: (event: { payload: unknown }) => void) => {
		h.listeners.set(name, handler);
		return () => h.listeners.delete(name);
	},
}));

const { GlowSurface } = await import("../src/components/GlowSurface.tsx");
const { App } = await import("../src/App.tsx");

function emit(name: string, payload: unknown) {
	act(() => h.listeners.get(name)?.({ payload }));
}

const glow = (payload: unknown) => emit("computer://glow", payload);
const session = (active: boolean) => emit("computer://state", { active, yolo: false });
const working = (active: boolean, operationId: number) => emit("computer://working", { active, operationId });

async function renderGlow() {
	const view = render(<GlowSurface />);
	await waitFor(() => expect(h.listeners.has("computer://glow")).toBe(true));
	await waitFor(() => expect(h.listeners.has("computer://state")).toBe(true));
	return view.container;
}

/** The overlay, once it also listens for the agent's working state. */
async function mountEdge() {
	const view = render(<GlowSurface />);
	await waitFor(() => expect(h.listeners.has("computer://glow")).toBe(true));
	await waitFor(() => expect(h.listeners.has("computer://state")).toBe(true));
	await waitFor(() => expect(h.listeners.has("computer://working")).toBe(true));
	return view;
}

async function renderEdge() {
	return (await mountEdge()).container;
}

const marks = (root: HTMLElement) => root.querySelectorAll(".glow-mark");
const edge = (root: HTMLElement) => root.querySelector(".glow-working");
const readies = () => h.invoke.mock.calls.filter(([command]) => command === "computer_working_ready");

/** Runs the animation frames the browser would paint next, including any they request. */
function paint(count = 2) {
	for (let i = 0; i < count; i += 1) {
		const due = [...h.frames.values()];
		h.frames.clear();
		act(() => {
			for (const callback of due) callback(0);
		});
	}
}

// A variable, so the bundler leaves the stylesheet's path alone.
const STYLES = "../src/styles.css";
const stylesheet = () => readFileSync(fileURLToPath(new URL(STYLES, import.meta.url)), "utf8");
const rule = (css: string, selector: string) =>
	css.match(new RegExp(`\\n${selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")} \\{([^}]*)\\}`))?.[1] ?? "";

const originalFrame = window.requestAnimationFrame;
const originalCancelFrame = window.cancelAnimationFrame;

beforeEach(() => {
	h.invoke.mockReset();
	h.invoke.mockImplementation(() => new Promise(() => {}));
	h.listeners.clear();
	h.reducedMotion = false;
	h.frames.clear();
	window.requestAnimationFrame = (callback: FrameRequestCallback) => {
		h.nextFrame += 1;
		h.frames.set(h.nextFrame, callback);
		return h.nextFrame;
	};
	window.cancelAnimationFrame = (frame: number) => {
		h.frames.delete(frame);
	};
	window.matchMedia = ((query: string) => ({
		matches: query.includes("reduced-motion") && h.reducedMotion,
		media: query,
		addEventListener: () => {},
		removeEventListener: () => {},
	})) as unknown as typeof window.matchMedia;
});

afterEach(() => {
	vi.useRealTimers();
	window.requestAnimationFrame = originalFrame;
	window.cancelAnimationFrame = originalCancelFrame;
	window.history.replaceState(null, "", "/");
});

describe("GlowSurface (S28)", () => {
	it("shows nothing while the agent is idle", async () => {
		const root = await renderGlow();
		expect(marks(root)).toHaveLength(0);
		session(true);
		expect(marks(root)).toHaveLength(0);
	});

	it("pulses where the agent clicked, in the overlay's points", async () => {
		const root = await renderGlow();
		glow({ kind: "click", x: 60, y: 120 });
		const click = root.querySelector(".glow-mark.glow-click") as HTMLElement;
		expect(click).not.toBeNull();
		expect(click.style.left).toBe("60px");
		expect(click.style.top).toBe("120px");
	});

	it("draws a trail along a drag and short markers for move and scroll", async () => {
		const root = await renderGlow();
		glow({ kind: "drag", x: 100, y: 100, toX: 400, toY: 500 });
		const trail = root.querySelector(".glow-trail") as HTMLElement;
		expect(trail).not.toBeNull();
		expect(trail.style.width).toBe("500px");
		expect(root.querySelector(".glow-mark.glow-drag")).not.toBeNull();
		glow({ kind: "move", x: 1, y: 2 });
		glow({ kind: "scroll", x: 3, y: 4 });
		expect(root.querySelector(".glow-mark.glow-move")).not.toBeNull();
		expect(root.querySelector(".glow-mark.glow-scroll")).not.toBeNull();
	});

	it("outlines the focused element for keyboard actions", async () => {
		const root = await renderGlow();
		glow({ kind: "key", x: 160, y: 100, width: 200, height: 30 });
		const field = root.querySelector(".glow-mark.glow-key") as HTMLElement;
		expect(field.style.width).toBe("200px");
		expect(field.style.height).toBe("30px");
	});

	it("ignores events it cannot place", async () => {
		const root = await renderGlow();
		for (const payload of [
			null,
			{ kind: "click" },
			{ kind: "explode", x: 1, y: 1 },
			{ kind: "key", x: 1, y: 1 },
			{ kind: "drag", x: 1, y: 1 },
			{ kind: "click", x: Number.NaN, y: 1 },
		]) {
			glow(payload);
		}
		expect(marks(root)).toHaveLength(0);
	});

	it("fades out about 1.5 s after the last action", async () => {
		const root = await renderGlow();
		vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
		glow({ kind: "click", x: 10, y: 10 });
		act(() => vi.advanceTimersByTime(1000));
		// Another action keeps it lit.
		glow({ kind: "move", x: 20, y: 20 });
		act(() => vi.advanceTimersByTime(1200));
		expect(marks(root)).toHaveLength(2);
		expect(root.querySelector(".glow-layer")).toHaveClass("glow-fading");
		act(() => vi.advanceTimersByTime(300));
		expect(marks(root)).toHaveLength(0);
	});

	it("disappears at once when the session ends and stays dark until the next one", async () => {
		const root = await renderGlow();
		session(true);
		glow({ kind: "click", x: 10, y: 10 });
		session(false);
		expect(marks(root)).toHaveLength(0);
		// A mark that arrives after Stop, panic, or the timeout is dropped.
		glow({ kind: "click", x: 10, y: 10 });
		expect(marks(root)).toHaveLength(0);
		session(true);
		glow({ kind: "click", x: 10, y: 10 });
		expect(marks(root)).toHaveLength(1);
	});

	it("stays still with reduced motion: a static glow, no pulse or trail", async () => {
		h.reducedMotion = true;
		const root = await renderGlow();
		glow({ kind: "drag", x: 100, y: 100, toX: 400, toY: 500 });
		expect(root.querySelector(".glow-layer")).toHaveClass("glow-static");
		expect(root.querySelector(".glow-trail")).toBeNull();
		const end = root.querySelector(".glow-mark.glow-drag") as HTMLElement;
		expect([end.style.left, end.style.top]).toEqual(["400px", "500px"]);
	});

	it("runs a neon light along the top edge while the agent works, with no text or logo, until that operation ends", async () => {
		const root = await renderEdge();
		expect(edge(root)).toBeNull();
		working(true, 1);
		const shown = edge(root) as HTMLElement;
		expect(shown).not.toBeNull();
		expect(shown.querySelector(".glow-working-light")).not.toBeNull();
		expect(shown.querySelector(".rose, .dot-disc, svg")).toBeNull();
		expect(shown).toHaveTextContent("");
		expect(shown.closest('[aria-hidden="true"]')).not.toBeNull();
		expect(root.querySelector("button, [role], [tabindex]")).toBeNull();
		working(false, 1);
		expect(edge(root)).toBeNull();
	});

	it("tells the app the working edge is up only once it has been painted", async () => {
		const root = await renderEdge();
		const seen: boolean[] = [];
		h.invoke.mockImplementation((command: string) => {
			if (command === "computer_working_ready") seen.push(edge(root) !== null);
			return new Promise(() => {});
		});
		working(true, 3);
		expect(readies()).toHaveLength(0);
		paint(1);
		expect(readies()).toHaveLength(0);
		paint(1);
		expect(readies()).toEqual([["computer_working_ready", { operationId: 3 }]]);
		expect(seen).toEqual([true]);
		paint();
		expect(readies()).toHaveLength(1);
	});

	it("ignores working events it cannot read", async () => {
		const root = await renderEdge();
		for (const payload of [
			null,
			1,
			{ active: true },
			{ operationId: 1 },
			{ active: "yes", operationId: 1 },
			{ active: true, operationId: "1" },
			{ active: true, operationId: -1 },
			{ active: true, operationId: 1.5 },
			{ active: true, operationId: Number.NaN },
			{ active: true, operationId: 2 ** 53 },
		]) {
			emit("computer://working", payload);
		}
		paint();
		expect(edge(root)).toBeNull();
		expect(readies()).toHaveLength(0);
		// Zero is a valid operation.
		working(true, 0);
		expect(edge(root)).not.toBeNull();
	});

	it("never lets an older or late operation revive or hide the current one", async () => {
		const root = await renderEdge();
		working(true, 2);
		working(false, 1);
		expect(edge(root)).not.toBeNull();
		working(true, 1);
		paint();
		expect(readies()).toEqual([["computer_working_ready", { operationId: 2 }]]);
		working(false, 2);
		expect(edge(root)).toBeNull();
		// A start that arrives after its own end stays dark.
		working(true, 2);
		expect(edge(root)).toBeNull();
		// An end that overtakes its start fences that start too.
		working(false, 4);
		working(true, 4);
		working(true, 3);
		paint();
		expect(edge(root)).toBeNull();
		expect(readies()).toHaveLength(1);
		working(true, 5);
		expect(edge(root)).not.toBeNull();
	});

	it("does not acknowledge an operation that ended before the working edge was painted", async () => {
		const root = await renderEdge();
		working(true, 1);
		working(false, 1);
		paint();
		expect(edge(root)).toBeNull();
		expect(readies()).toHaveLength(0);
	});

	it("clears the working edge when the session ends, and a late start stays dark until the next session", async () => {
		const root = await renderEdge();
		session(true);
		working(true, 1);
		session(false);
		paint();
		expect(edge(root)).toBeNull();
		expect(readies()).toHaveLength(0);
		working(true, 2);
		paint();
		expect(edge(root)).toBeNull();
		expect(readies()).toHaveLength(0);
		session(true);
		working(true, 3);
		paint();
		expect(edge(root)).not.toBeNull();
		expect(readies()).toEqual([["computer_working_ready", { operationId: 3 }]]);
	});

	it("drops a pending acknowledgement when the overlay goes away", async () => {
		const view = await mountEdge();
		working(true, 1);
		view.unmount();
		paint();
		expect(readies()).toHaveLength(0);
	});

	it("keeps the working edge lit while the action marks fade", async () => {
		const root = await renderEdge();
		vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
		working(true, 1);
		glow({ kind: "click", x: 10, y: 10 });
		expect(root.querySelector(".glow-layer .glow-working")).toBeNull();
		act(() => vi.advanceTimersByTime(1200));
		expect(root.querySelector(".glow-layer")).toHaveClass("glow-fading");
		expect(edge(root)).not.toBeNull();
		act(() => vi.advanceTimersByTime(300));
		expect(marks(root)).toHaveLength(0);
		expect(edge(root)).not.toBeNull();
	});

	it("keeps the edge lit and still with reduced motion", async () => {
		h.reducedMotion = true;
		const root = await renderEdge();
		working(true, 1);
		expect(edge(root)).toHaveClass("glow-static");
		paint();
		expect(readies()).toHaveLength(1);
		const css = stylesheet();
		expect(rule(css, ".glow-static .glow-working-light")).toMatch(/display: none/);
	});

	it("styles the edge in the rose's neon, click-through, with a light that travels along it", () => {
		const css = stylesheet();
		expect(rule(css, ".glow-working")).toMatch(/pointer-events: none/);
		expect(rule(css, ".glow-working")).toMatch(/top: 0/);
		expect(rule(css, ".glow-working")).toMatch(/#ff2d7a/);
		expect(rule(css, ".glow-working-light")).toMatch(/animation: glow-travel/);
		expect(css).toMatch(/@keyframes glow-travel \{/);
		expect(css).not.toMatch(/\.glow-ring/);
	});

	it("is the app's glow window, which never connects to the assistant", async () => {
		window.history.replaceState(null, "", "/?surface=glow");
		const view = render(<App />);
		await waitFor(() => expect(h.listeners.has("computer://glow")).toBe(true));
		expect(view.container.querySelector(".glow-layer")).not.toBeNull();
		expect(document.documentElement.dataset.surface).toBe("glow");
		expect(h.invoke).not.toHaveBeenCalledWith("connection_info", undefined);
	});
});
