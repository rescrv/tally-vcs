/**
 * abelian text-editor override (proof of concept)
 * ------------------------------------------------
 * Overrides pi's built-in `edit` and `write` tools by registering tools with
 * the same names. Instead of touching the filesystem directly, every mutation
 * is routed through the purpose-built `abelian-pi-editor` hook binary, which
 * turns the tool call into an abelian patch: exact-match precondition, sum
 * arithmetic, durable log append with the observed read set, and a
 * best-effort working-tree refresh.
 *
 * The model-facing schemas are identical to the built-ins, so nothing about
 * how the agent is prompted changes — only where the bytes land.
 *
 * Setup
 * -----
 * Requires an abelian repository in the project (`abelian init`) and the hook
 * binary. Build it with the `pi` feature (it is gated behind it):
 *   cargo build --features pi --bin abelian-pi-editor
 * The extension looks for the binary at `target/{release,debug}/abelian-pi-editor`
 * under the project root, then on PATH. Override the location with
 * `ABELIAN_PI_EDITOR=/path/to/abelian-pi-editor`.
 *
 * If the binary cannot be found the extension warns once at session start and
 * registers degraded `edit`/`write` tools that return a clear, actionable
 * error instead of failing per call with an opaque launch error. Set
 * `ABELIAN_FALLBACK_NATIVE=1` to instead fall through to Pi's native
 * `edit`/`write` so the agent is not bricked while the binary is missing.
 *
 * Divergence from Pi's built-in `edit`: abelian replaces only the matched span
 * and keeps every untouched byte byte-identical; it does NOT reproduce Pi's
 * whole-file normalization write-back that rewrites unchanged lines. This is
 * intentional and stricter, and better for the substrate.
 */

import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { createEditToolDefinition, createWriteToolDefinition } from "@earendil-works/pi-coding-agent";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { delimiter, join } from "node:path";
import { Type } from "typebox";

// ---- schemas: mirror pi's built-in edit/write tools ----

const replaceEditSchema = Type.Object({
	oldText: Type.String({
		description:
			"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.",
	}),
	newText: Type.String({ description: "Replacement text for this targeted edit." }),
});

const editSchema = Type.Object({
	path: Type.String({ description: "Path to the file to edit (relative or absolute)" }),
	edits: Type.Array(replaceEditSchema, {
		description: "One or more targeted replacements applied against the original file.",
	}),
});

const writeSchema = Type.Object({
	path: Type.String({ description: "Path to the file to write (relative or absolute)" }),
	content: Type.String({ description: "Content to write to the file" }),
});

// ---- legacy argument shim for `edit` ----
//
// Pi's built-in `edit` uses `prepareArguments` (docs/extensions.md "Argument
// preparation") to fold legacy top-level `oldText`/`newText` into `edits[]`
// when resuming an old session, and to parse `edits` sent as a JSON string by
// some models. Our override must do the same or resuming a pre-`edits[]`
// session onto this extension throws on those calls. Mirrors
// `prepareEditArguments` in Pi's `core/tools/edit.js`. `write` needs no shim
// (its shape is unchanged), so only the `edit` tool registers this.
function prepareEditArguments(input: unknown): unknown {
	if (!input || typeof input !== "object") {
		return input;
	}
	const args = input as Record<string, unknown>;
	// Some models (Opus 4.6, GLM-5.1) send edits as a JSON string, not an array.
	if (typeof args.edits === "string") {
		try {
			const parsed = JSON.parse(args.edits);
			if (Array.isArray(parsed)) args.edits = parsed;
		} catch {
			// leave as-is; schema validation will reject it
		}
	}
	const legacy = args as { oldText?: unknown; newText?: unknown; edits?: unknown };
	if (typeof legacy.oldText !== "string" || typeof legacy.newText !== "string") {
		return args;
	}
	const edits = Array.isArray(legacy.edits) ? [...legacy.edits] : [];
	edits.push({ oldText: legacy.oldText, newText: legacy.newText });
	const { oldText: _oldText, newText: _newText, ...rest } = legacy;
	return { ...rest, edits };
}

// ---- locate the hook binary ----

// Returns the resolved binary path, or null if it cannot be located anywhere
// we know to look (explicit override, project target dirs, PATH).
function resolveHook(cwd: string): string | null {
	const env = process.env.ABELIAN_PI_EDITOR;
	if (env) return existsSync(env) ? env : null;
	for (const candidate of [
		join(cwd, "target", "release", "abelian-pi-editor"),
		join(cwd, "target", "debug", "abelian-pi-editor"),
	]) {
		if (existsSync(candidate)) return candidate;
	}
	// Fall back to PATH, but verify it is actually present so we can fail loudly
	// at load rather than opaquely per call.
	for (const dir of (process.env.PATH ?? "").split(delimiter)) {
		if (dir && existsSync(join(dir, "abelian-pi-editor"))) return join(dir, "abelian-pi-editor");
	}
	return null;
}

// ---- prose: the model's between-call narrative becomes the patch log ----
//
// Between tool calls the model writes plain-language prose explaining what it
// is about to do; that prose is the change's narrative. But one assistant turn
// can issue several tool calls, and lifting the whole message text onto *every*
// resulting patch stamps the identical, over-broad narrative on unrelated
// changes. So we attribute the prose only once per assistant turn: the first
// mutation of a turn carries it; siblings in the same turn carry none (their
// narrative is the empty span, honestly attributing the prose to one change).

/** The latest assistant message's id and its concatenated text, or null. */
function latestAssistantProse(
	ctx: { sessionManager: { getBranch?: () => unknown[]; getEntries?: () => unknown[] } },
): { id: string; text: string } | null {
	const entries =
		(ctx.sessionManager.getBranch?.() ?? ctx.sessionManager.getEntries?.() ?? []) as Array<{
			type?: string;
			id?: string;
			message?: { role?: string; content?: unknown };
		}>;
	for (let i = entries.length - 1; i >= 0; i--) {
		const e = entries[i];
		if (e?.type !== "message" || e.message?.role !== "assistant") continue;
		const content = e.message.content;
		if (!Array.isArray(content)) return null;
		const text = content
			.filter((p): p is { type: "text"; text: string } => (p as { type?: string })?.type === "text")
			.map((p) => p.text)
			.join("")
			.trim();
		if (!text) return null;
		return { id: e.id ?? String(i), text };
	}
	return null;
}

// ---- invoke the hook, piping the tool-call JSON on stdin ----

function runHook(
	bin: string,
	subcommand: "edit" | "write",
	input: unknown,
	cwd: string,
	sessionId: string | undefined,
	prose: string | undefined,
	signal: AbortSignal | undefined,
): Promise<{ stdout: string; stderr: string; code: number }> {
	return new Promise((resolve, reject) => {
		const child = spawn(bin, [subcommand], {
			cwd,
			env: {
				...process.env,
				// abelian treats sessions and forks as the same object.
				...(sessionId ? { PI_SESSION_ID: sessionId, ABELIAN_FORK: sessionId } : {}),
				...(prose ? { ABELIAN_PROSE: prose } : {}),
			},
			signal,
		});
		let stdout = "";
		let stderr = "";
		child.stdout.on("data", (d) => (stdout += d.toString()));
		child.stderr.on("data", (d) => (stderr += d.toString()));
		child.on("error", reject);
		child.on("close", (code) => resolve({ stdout, stderr, code: code ?? -1 }));
		child.stdin.end(JSON.stringify(input));
	});
}

export default function (pi: ExtensionAPI) {
	// Resolve the hook binary once, at load/session start, rather than on every
	// edit. `hookBin` is null when the binary cannot be found; in that case the
	// tools run in a degraded mode (see below). Cached and refreshed on each
	// session_start so a mid-session `cargo build` is picked up on next session.
	let hookBin: string | null = null;
	const fallbackNative = process.env.ABELIAN_FALLBACK_NATIVE === "1";
	// The assistant message id whose prose has already been attributed to a
	// patch this turn. Ensures the between-call narrative lands on exactly one
	// change even when a turn issues several edits/writes.
	let lastAttributedAssistantId: string | null = null;

	pi.on("session_start", (_event, ctx) => {
		hookBin = resolveHook(ctx.cwd);
		if (!hookBin) {
			ctx.ui.notify(
				fallbackNative
					? "abelian-pi-editor not found; falling back to native edit/write. Build it with: cargo build --features pi --bin abelian-pi-editor"
					: "abelian-pi-editor not found; edit/write are degraded. Build it with: cargo build --features pi --bin abelian-pi-editor (or set ABELIAN_FALLBACK_NATIVE=1)",
				"warn",
			);
		}
	});

	const register = (
		name: "edit" | "write",
		parameters: unknown,
		prepareArguments?: (args: unknown) => unknown,
	) => {
		// Lazily built native tool definition, used only when the hook is missing
		// and ABELIAN_FALLBACK_NATIVE=1.
		let nativeTool: ToolDefinition | undefined;
		const native = (cwd: string): ToolDefinition => {
			if (!nativeTool) {
				nativeTool = (name === "edit"
					? createEditToolDefinition(cwd)
					: createWriteToolDefinition(cwd)) as unknown as ToolDefinition;
			}
			return nativeTool;
		};

		pi.registerTool({
			name, // same name as built-in → overrides it
			label: `${name} (abelian)`,
			...(prepareArguments ? { prepareArguments } : {}),
			description:
				name === "edit"
					? "Edit a file via the abelian substrate using exact text replacement (edits[].oldText must be unique). The change is recorded as a patch with its observed read set."
					: "Write a file via the abelian substrate (create or whole-file overwrite). The change is recorded as a patch with its observed read set.",
			parameters: parameters as never,
			async execute(toolCallId, params, signal, onUpdate, ctx) {
				const bin = hookBin ?? resolveHook(ctx.cwd);
				if (!bin) {
					if (fallbackNative) {
						return native(ctx.cwd).execute(toolCallId, params as never, signal, onUpdate as never, ctx);
					}
					return {
						content: [
							{
								type: "text",
								text: `abelian ${name} unavailable: abelian-pi-editor binary not found. Build it with: cargo build --features pi --bin abelian-pi-editor (or set ABELIAN_PI_EDITOR / ABELIAN_FALLBACK_NATIVE=1).`,
							},
						],
						details: { error: true },
						isError: true,
					};
				}
				const sessionId = ctx.sessionManager.getSessionId?.();
				// Attribute the assistant's between-call prose to the first mutation of
				// the turn only; later siblings carry no prose.
				let prose: string | undefined;
				const latest = latestAssistantProse(ctx);
				if (latest && latest.id !== lastAttributedAssistantId) {
					lastAttributedAssistantId = latest.id;
					prose = latest.text;
				}
				let result: Awaited<ReturnType<typeof runHook>>;
				try {
					result = await runHook(bin, name, params, ctx.cwd, sessionId, prose, signal);
				} catch (err) {
					return {
						content: [{ type: "text", text: `abelian ${name} failed to launch: ${String(err)}` }],
						details: { error: true },
						isError: true,
					};
				}
				if (result.code !== 0) {
					return {
						content: [
							{ type: "text", text: result.stderr.trim() || `abelian ${name} exited ${result.code}` },
						],
						details: { error: true, code: result.code },
						isError: true,
					};
				}
				let parsed: Record<string, unknown> = {};
				try {
					parsed = JSON.parse(result.stdout.trim() || "{}");
				} catch {
					// non-fatal; fall through with raw output
				}
				const summary =
					name === "edit"
						? `edited ${parsed.path ?? (params as { path: string }).path}`
						: `wrote ${parsed.path ?? (params as { path: string }).path}`;
				return {
					content: [
						{ type: "text", text: `${summary}\nabelian ${parsed.id ?? ""} sum=${parsed.sum ?? ""}`.trim() },
					],
					details: parsed,
				};
			},
		});
	};

	register("edit", editSchema, prepareEditArguments);
	register("write", writeSchema);
}
