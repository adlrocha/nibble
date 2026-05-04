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
