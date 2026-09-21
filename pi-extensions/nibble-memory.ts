/**
 * Nibble Memory Extension for Pi
 *
 * Live-captures session events to the nibble memory system, mirroring
 * Claude Code's hook-based capture:
 *   - input:         user messages
 *   - message_end:   assistant messages
 *   - tool_call:     records tool inputs (paired with tool_execution_end)
 *   - tool_execution_end: records tool outputs
 *   - session_shutdown: reports status → exited and triggers async summarization
 *   - session_start:   reports the session file path to nibble (eager
 *                      task→session mapping so attach never has to guess
 *                      which session belongs to this task after a reboot)
 *   - agent_start:     status → running (also retries session-path report)
 *   - agent_settled:   status → completed (only when the session is idle,
 *                      so subagent/workflow activity doesn't flap the state)
 *
 * Events are written to ~/.nibble/memory/capture/<project>/<task-id>.jsonl
 * and later processed by `nibble memory summarize <task-id>`.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { execSync } from "child_process";

// ─── Helpers ─────────────────────────────────────────────────────────────────

const getTaskId = (): string => process.env.AGENT_TASK_ID ?? "";

const capture = (
	taskId: string,
	role: string,
	content: string,
	extra?: Record<string, string>,
): void => {
	if (!taskId) return;

	const args = ["memory", "capture", taskId, role, content];
	for (const [k, v] of Object.entries(extra ?? {})) {
		args.push(`--${k}`, v);
	}

	try {
		execSync(
			`nibble ${args.map((a) => `'${a.replace(/'/g, "'\\''")}'`).join(" ")}`,
			{ timeout: 5000, stdio: "pipe" },
		);
	} catch {
		// Non-fatal: capture is best-effort
	}
};

const summarize = (taskId: string): void => {
	if (!taskId) return;

	try {
		execSync(`nibble memory summarize '${taskId.replace(/'/g, "'\\''")}'`, {
			timeout: 120000,
			stdio: "pipe",
			env: { ...process.env, NIBBLE_AGENT_TYPE: "pi" },
		});
	} catch {
		// Non-fatal: summarization may fail if LLM is down
	}
};

const reportSessionPath = (taskId: string, path: string): void => {
	if (!taskId || !path) return;

	try {
		execSync(
			`nibble report session-path '${taskId.replace(/'/g, "'\\''")}' '${path.replace(/'/g, "'\\''")}'`,
			{ timeout: 5000, stdio: "pipe" },
		);
	} catch {
		// Non-fatal: session-path reporting is best-effort
	}
};

const reportStatus = (taskId: string, state: string): void => {
	if (!taskId) return;

	try {
		execSync(`nibble report status '${taskId.replace(/'/g, "'\\''")}' '${state}'`, {
			timeout: 5000,
			stdio: "pipe",
		});
	} catch {
		// Non-fatal: status reporting is best-effort
	}
};

// ─── Extension ───────────────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {
	// Store tool inputs by toolCallId so we can pair them with results.
	const toolInputs = new Map<
		string,
		{ name: string; input: string }
	>();

	// ── input: capture user messages ───────────────────────────────────────
	pi.on("input", async (event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		if (event.text?.trim()) {
			capture(taskId, "user", event.text);
		}
	});

	// ── message_end: capture assistant messages ────────────────────────────
	pi.on("message_end", async (event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;

		const msg = event.message;
		if (msg?.role !== "assistant") return;

		const text = Array.isArray(msg.content)
			? msg.content
				.filter((b: any) => b.type === "text")
				.map((b: any) => b.text)
				.join("\n")
			: String(msg.content);

		if (text.trim()) {
			capture(taskId, "assistant", text);
		}
	});

	// ── tool_call: record tool input ───────────────────────────────────────
	pi.on("tool_call", async (event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;

		toolInputs.set(event.toolCallId, {
			name: event.toolName,
			input: JSON.stringify(event.input),
		});
	});

	// ── tool_execution_end: capture tool result ────────────────────────────
	pi.on("tool_execution_end", async (event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;

		const toolInfo = toolInputs.get(event.toolCallId);
		if (!toolInfo) return;

		const output = Array.isArray(event.result?.content)
			? event.result.content
				.filter((b: any) => b.type === "text")
				.map((b: any) => b.text)
				.join("\n")
			: String(event.result?.content ?? "");

		capture(taskId, "tool", "", {
			"tool-name": toolInfo.name,
			"tool-input": toolInfo.input,
			"tool-output": output,
		});

		toolInputs.delete(event.toolCallId);
	});

	// ── session_start: report the session file path to nibble ────────────
	// Fires on both fresh and resumed sessions (also when the user switches
	// sessions mid-run via /resume), so the DB mapping always tracks the
	// session this task is actually in. --btw side sessions have no
	// AGENT_TASK_ID and correctly no-op here.
	const reportCurrentSession = (ctx: unknown): void => {
		const taskId = getTaskId();
		if (!taskId) return;
		const file = (ctx as any)?.sessionManager?.getSessionFile?.();
		if (typeof file === "string" && file) {
			reportSessionPath(taskId, file);
		}
	};
	// ── agent_settled: status → completed (only when truly idle) ───────────
	// agent_settled also fires while subagents/queued retries/compaction are
	// active; only mark idle when the session reports itself idle so the
	// status doesn't flap mid-run.
	pi.on("agent_settled", async (_event, ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		const isIdle = (ctx as { isIdle?: () => boolean } | null)?.isIdle;
		if (typeof isIdle === "function" && isIdle.call(ctx) === false) return;
		reportStatus(taskId, "completed");
	});

	// ── session_start: eager task→session mapping ─────────────────────────
	// Fires on fresh and resumed sessions (also on mid-run /resume switches)
	// so the DB mapping always tracks the session this task is actually in.
	pi.on("session_start", async (_event, ctx) => reportCurrentSession(ctx));

	// Fallback for agents whose session_start fires before the session file
	// path is known: retry on the first agent turn. Also flips live status to
	// running for the nibble status sidebar.
	pi.on("agent_start", async (_event, ctx) => {
		const taskId = getTaskId();
		if (taskId) reportStatus(taskId, "running");
		reportCurrentSession(ctx);
	});
	pi.on("session_shutdown", async (_event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;

		reportStatus(taskId, "exited");

		// Summarize in background so the agent exits cleanly
		setTimeout(() => summarize(taskId), 500);
	});

	// Log that the extension loaded
	console.log(
		"[nibble-memory] Pi extension loaded — live capture + summarization active",
	);
}
