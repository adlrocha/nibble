/**
 * Nibble Memory Extension for Pi
 *
 * Live-captures session events to the nibble memory system, mirroring
 * Claude Code's hook-based capture:
 *   - input:         user messages
 *   - message_end:   assistant messages
 *   - tool_call:     records tool inputs (paired with tool_execution_end)
 *   - tool_execution_end: records tool outputs
 *   - session_shutdown: triggers async session summarization
 *   - tool_approval_requested/resolved: reports the agent blocked (with the
 *                      approval reason) while it waits on a permission
 *                      decision, and working again once resolved
 *   - session_start:   reports the session file path to nibble (eager
 *                      task→session mapping so attach never has to guess
 *                      which session belongs to this task after a reboot)
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

const reportStatus = (taskId: string, state: string, message?: string): void => {
	if (!taskId) return;

	const args = ["report", "status", taskId, state];
	if (message?.trim()) {
		args.push("--message", message.trim());
	}

	try {
		execSync(
			`nibble ${args.map((a) => `'${a.replace(/'/g, "'\\''")}'`).join(" ")}`,
			{ timeout: 5000, stdio: "pipe" },
		);
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

	// ── input: capture user messages, mark the agent working ──────────────
	pi.on("input", async (event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		reportStatus(taskId, "working");
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

	// ── tool_execution_end: capture tool result, mark the agent working ───
	// (also clears a blocked marker once a decision is acted on)
	pi.on("tool_execution_end", async (event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		reportStatus(taskId, "working");
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

	// ── tool_approval_requested/resolved: agent is waiting on the user ───
	// omp only emits these when an extension subscribes, so registering the
	// handlers is what turns them on. Track concurrent approvals by call id
	// so one resolution does not clear the blocked marker while another
	// prompt is still open. Payload (observability event): { sessionId,
	// toolName, toolCallId, reason?, approvalMode? } / { ..., approved }.
	const pendingApprovals = new Map<string, string>();

	const approvalKey = (event: unknown): string =>
		event && typeof event === "object" && "toolCallId" in event &&
		typeof event.toolCallId === "string"
			? event.toolCallId
			: "unknown";

	pi.on("tool_approval_requested", async (event: unknown, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		const label =
			event && typeof event === "object"
				? "reason" in event &&
					typeof event.reason === "string" &&
					event.reason.trim()
					? event.reason.trim()
					: "toolName" in event && typeof event.toolName === "string"
						? `approve ${event.toolName}`
						: "approval needed"
				: "approval needed";
		pendingApprovals.set(approvalKey(event), label);
		reportStatus(taskId, "blocked", label);
	});
	pi.on("tool_approval_resolved", async (event: unknown, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		pendingApprovals.delete(approvalKey(event));
		const remaining = [...pendingApprovals.values()].pop();
		if (remaining) {
			reportStatus(taskId, "blocked", remaining);
		} else {
			reportStatus(taskId, "working");
		}
	});

	// ── agent_end: run finished, mark the agent idle ────────────────────
	// turn_end fires at every model-turn boundary — tool follow-ups
	// continue in a new turn — so reporting idle there shows working
	// agents as idle between turns. agent_end is the true run boundary.
	pi.on("agent_end", async (_event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;
		pendingApprovals.clear();
		reportStatus(taskId, "idle");
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
	pi.on("session_start", async (_event, ctx) => reportCurrentSession(ctx));
	// Fallback for agents whose session_start fires before the session file
	// path is known: retry on the first agent turn.
	pi.on("agent_start", async (_event, ctx) => reportCurrentSession(ctx));

	// ── session_shutdown: trigger summarization ────────────────────────────
	pi.on("session_shutdown", async (_event, _ctx) => {
		const taskId = getTaskId();
		if (!taskId) return;

		// Summarize in background so Pi exits cleanly
		setTimeout(() => summarize(taskId), 500);
	});

	// Log that the extension loaded
	console.log(
		"[nibble-memory] Pi extension loaded — live capture + summarization active",
	);
}
