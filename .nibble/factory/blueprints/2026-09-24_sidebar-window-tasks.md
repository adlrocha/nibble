# Sidebar accuracy: per-window sandbox tasks + self-healing registration

Date: 2026-09-24 · Tier: Standard · Retrospective scope: delta only

## Problem

Three defects make the `nibble status` sidebar misrepresent live agents:

1. **P1 — shared task identity per sandbox.** `cmd_sandbox_attach`
   (`src/sandbox/commands.rs:1637`) and `cmd_hermes_attach`
   (`src/sandbox/hermes.rs:290`) inject the sandbox task's ID as
   `AGENT_TASK_ID` into every window. N windows → 1 sidebar row; status
   reports flap (one window settling flips the shared row to idle while
   another works; one window exiting marks the row exited while others live).
2. **P2 — silent registration loss.** Wrappers run
   `nibble report start … 2>/dev/null || true`; the omp/pi extension and
   Claude hooks likewise swallow every `nibble report` failure. One transient
   DB failure at start (reproduced: `database is locked` under concurrent
   `status --watch` / `web` writers) makes the session permanently invisible —
   later reports hit "Task not found" and are swallowed too.
3. **P3 — misleading display.** Window rows would render as `host` location;
   sandbox rows show `running` (green) even with no attached window.

## Change

### A. Window tasks (per attach invocation)

- New task row per `cmd_sandbox_attach` / `cmd_hermes_attach` invocation
  (including `--btw` — side sessions get their own row; nothing is shared any
  more, so the old clobbering reason is gone). Covers spawn-attach
  (`cmd_sandbox_spawn` calls `cmd_sandbox_attach`).
- Fields: fresh UUID; agent_type mapped from SelectedAgent
  (Claude→ClaudeCode, Pi→Pi, Omp→Unknown("omp"), Hermes→Hermes);
  title `{parent_title} · {agent} #{N}` (N = existing children + 1);
  `pid = std::process::id()` — attach `exec`s into podman, so the pid lives
  exactly as long as the window and host pid-reconcile auto-marks exit;
  `container_name = None` (keeps reconcile applicable);
  `context.project_path` = parent's; `context.extra`: `parent_task_id`,
  `zellij_pane_id`, `window: true`, plus `btw: true` for side sessions.
- `AGENT_TASK_ID` passed to `podman exec` = window task ID.
- Sandbox (parent) task stays the container row: initial status at spawn
  becomes `Completed` (idle, container up) instead of `Running`; exits via
  existing prune/kill paths. Parent pane-id write stays (goto → last window).
- Session resume: attach resolves `claude_session_id` / stored
  `pi_session_path` from parent first, then from the most recent non-btw
  child window (hooks now write session data to the window task).

### B. Self-healing registration (in `nibble report`, one fix point)

- `report status <unknown-id> running|blocked|completed` → auto-create a
  placeholder task (agent from `NIBBLE_AGENT_TYPE` env, title
  `[{basename(cwd)}]`, `project_path` = cwd), then apply the transition.
- `report session-id|session-path <unknown-id>` → same auto-create, then store.
- `report status <unknown-id> exited` → no-op success (agent already gone).
- Placeholder rows carry no pid (container pid-namespace hazard), rely on
  hook-reported exit and the 1h exited-row cutoff.

### C. Wrapper robustness

- omp/pi/claude/TEMPLATE wrappers: retry `report start` ×3 (1s backoff); on
  final failure print a visible stderr warning (never fatal, never silent).
- Repo omp-wrapper reports agent_type `omp` (matches installed wrapper and
  sidebar label), not `pi`.

### D. Extension labeling

- `pi-extensions/nibble-memory.ts`: set `NIBBLE_AGENT_TYPE` (omp vs pi,
  detected from the running binary path) in the env of every `nibble`
  execSync so auto-created placeholders get the right agent label.

### E. Rendering

- `render_status`: window tasks (parent_task_id present) show location
  `sandbox` instead of `host`.

## Non-goals

- Zombie reaping inside containers (`--init`), sidebar tree/grouped layout,
  container-state reconcile for sandbox rows (prune already covers), any web
  UI changes beyond picking up new rows automatically.

## Invariants

- INV-1: Every attach invocation yields exactly one new window task whose
  `AGENT_TASK_ID` is unique to that window.
- INV-2: Closing a window (podman exec exit) must surface as `exited` for
  that window row within one reconcile tick, without touching the sandbox
  row's status.
- INV-3: A status/session report for an unregistered ID must create a
  visible placeholder row, never fail silently.
- INV-4: `--btw` sessions must never influence the parent's stored session
  IDs used by later attaches.
- INV-5: Wrappers must never make the agent fail to start; degraded
  tracking must be visible (stderr) not silent.

## Verification

- Unit: window-task builder (field mapping, title numbering), child-session
  lookup (recency, btw exclusion), ensure-task-or-create, exited no-op,
  render location.
- Live smoke: scratch sandbox attach under `script`/`timeout` → window row
  appears running then reconciles exited; sandbox row stays idle; second
  concurrent window gets its own row. Self-heal probe via unknown UUID.
- Existing `cargo test` suite green; release build clean.
