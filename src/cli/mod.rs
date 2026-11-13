use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nibble")]
#[command(about = "Manage sandboxed coding agents", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum SdlcAction {
    /// Open a PR that installs nibble-review with land in shadow mode
    Install {
        /// Repositories as owner/name (default: the one in the current directory)
        repos: Vec<String>,
        /// Show what would change without opening a PR
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
        /// nibble-review tag, branch, or commit to pin (default: latest release)
        #[arg(long = "ref")]
        action_ref: Option<String>,
        /// Record that main deploys to dev and a release deploys to production
        #[arg(long)]
        main_is_staging: bool,
    },
    /// Open a PR that removes the flow, keeping the reviewer unless told otherwise
    Uninstall {
        /// Repositories as owner/name (default: the one in the current directory)
        repos: Vec<String>,
        /// Show what would change without opening a PR
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
        /// Also delete the nibble-review workflow
        #[arg(long)]
        remove_reviewer: bool,
    },
    /// Report drift between the default branch and the expected setup
    Check {
        /// Repositories as owner/name (default: the one in the current directory)
        repos: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum Commands {
    // ── Sandbox subcommands ────────────────────────────────────────────────────
    /// Manage sandboxed agent containers
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },

    /// Manage the Hermes Agent sandbox (singleton with dynamic repo mounts)
    Hermes {
        #[command(subcommand)]
        action: HermesAction,
    },

    /// Prune stale tasks (dead/gone containers → mark exited, GC old exited tasks)
    Prune,

    /// Show live agent statuses (sidebar-friendly; use --watch in a zellij pane)
    Status {
        /// Re-render every second (for a dedicated zellij side pane)
        #[arg(short, long)]
        watch: bool,
        /// Output machine-readable JSON (implies no watch)
        #[arg(long)]
        json: bool,
        /// Include exited tasks (dimmed at the bottom)
        #[arg(short, long)]
        all: bool,
        /// Delete exited task rows. Does not stop containers.
        #[arg(long)]
        clear: bool,
        /// Stop every sandbox and delete every task row.
        #[arg(long)]
        scratch: bool,
    },

    /// Open the agent-status side panel in the current zellij session
    Sidebar {
        /// Install the always-on sidebar layout (every tab of every new
        /// zellij session gets the tab bar and a status pane). Re-run after
        /// updating nibble to refresh it. Never overwrites a custom layout.
        #[arg(long)]
        install: bool,
        /// Remove the nibble-managed zellij layout files
        #[arg(long)]
        uninstall: bool,
        /// Focus the sidebar pane, opening it first if it is not open.
        #[arg(long, conflicts_with = "close")]
        focus: bool,
        /// Close the sidebar pane in this tab.
        #[arg(long)]
        close: bool,
    },

    /// Manage nibble's zellij keybindings (Alt a: sidebar, Alt v: vert)
    Keybinds {
        /// Add the bindings to `$ZELLIJ_CONFIG_FILE`, or
        /// `~/.config/zellij/config.kdl` when unset (re-running replaces
        /// them; bindings outside nibble's section are untouched)
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Remove nibble's section from config.kdl
        #[arg(long)]
        uninstall: bool,
    },

    /// Open vert on the repo in the current directory (the Alt v pane)
    Vert,

    /// Install, check, or remove the delivery flow in one or more repositories
    Sdlc {
        #[command(subcommand)]
        action: SdlcAction,
    },

    /// Open a named zellij session whose agents come back after a crash
    ///
    /// Attaches when the session is running. When it is gone (killed,
    /// crashed, rebooted), rebuilds its agent tabs and resumes each agent's
    /// conversation. Run inside a session of that name to start recording it.
    Mux {
        /// Zellij session name
        #[arg(default_value = crate::mux::DEFAULT_SESSION)]
        name: String,
        /// Print the recorded agent tabs instead of opening the session
        #[arg(long, conflicts_with_all = ["forget", "watch", "resume"])]
        show: bool,
        /// Drop nibble's record of the session's agents (refused while the
        /// session runs; zellij may still resurrect its own saved copy)
        #[arg(long, conflicts_with_all = ["watch", "resume"])]
        forget: bool,
        /// Internal: run the snapshot watcher for the session
        #[arg(long, hide = true, conflicts_with = "resume")]
        watch: bool,
        /// Internal: resume one recorded agent (run by restored panes)
        #[arg(long, hide = true, value_name = "TASK_ID")]
        resume: Option<String>,
    },

    /// Inject a message into a running sandbox agent (bypasses Telegram)
    Inject {
        /// Task ID of the agent to inject into
        task_id: String,
        /// Message to send
        message: String,
    },

    /// Run the quota auto-continue daemon (watch for subscription quota
    /// errors in agent sessions and continue the tasks once the quota resets)
    QuotaWatch {
        /// Run one scan-and-attempt pass and exit (testing / manual trigger)
        #[arg(long)]
        once: bool,
    },

    /// Manage scheduled cron jobs for sandboxes
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },

    /// List and manage local LLM model files
    Lm {
        #[command(subcommand)]
        action: LmAction,
    },

    /// Jump to the zellij pane hosting an agent (task ID or unique prefix)
    Goto {
        /// Task ID, unique ID prefix, or row number from `nibble status`
        task: String,
    },

    Report {
        #[command(subcommand)]
        action: ReportAction,
    },

    /// Manage persistent cross-session memory
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },

    /// List and inspect agent sessions (diagnostic)
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },

    /// Backup all nibble state to a zip file
    Backup {
        /// Output path for the zip file (defaults to nibble-backup-<timestamp>.zip)
        #[arg(short, long)]
        output: Option<String>,
        /// Also include agent session transcripts (~/.pi, ~/.omp and ~/.claude
        /// session dirs). These are kept forever so you can resume any
        /// conversation after a restore; can make the zip large.
        #[arg(long)]
        sessions: bool,
    },

    /// Import nibble state from a backup zip file
    Import {
        /// Path to the backup zip file
        path: String,
    },

    /// Track token usage across Claude Code and pi sessions
    Usage {
        #[command(subcommand)]
        action: UsageAction,
    },

    /// Run the web session inspector UI (dashboard, search, session review)
    Web {
        /// Address to bind (use 127.0.0.1 to restrict to localhost)
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Port to listen on
        #[arg(long, default_value_t = 7878)]
        port: u16,

        /// Bearer token required to access the UI (default: $NIBBLE_WEB_TOKEN)
        #[arg(long, env = "NIBBLE_WEB_TOKEN")]
        token: Option<String>,
    },

    /// Remote-control agent sessions from Slack (see docs/rc.md)
    Rc {
        #[command(subcommand)]
        action: RcAction,
    },
}

#[derive(Subcommand)]
pub enum RcAction {
    /// Store the Slack app's tokens and your member ID (prompts)
    Setup,
    /// Check credentials, Socket Mode, zellij and the daemon
    Doctor,
    /// Print the Slack app manifest to create the app from
    Manifest,
    /// Run the Slack daemon (normally nibble-rc.service)
    Daemon,
    /// Send a session to Slack (default: this agent's, from AGENT_TASK_ID)
    Start {
        /// Task ID (from `nibble status --json`)
        task_id: Option<String>,
    },
    /// Stop relaying a session to Slack; the session itself keeps running
    Stop {
        /// Task ID (default: AGENT_TASK_ID)
        task_id: Option<String>,
    },
    /// List sessions sent to or started from Slack
    List,
    /// Claude Code UserPromptSubmit hook: handles `/nibble-rc` without a model turn
    #[command(name = "claude-hook", hide = true)]
    ClaudeHook,
}

#[derive(Subcommand)]
pub enum UsageAction {
    /// Scan local CLI session logs and upsert per-message token usage rows
    Scan {
        /// Print a summary after scanning
        #[arg(long)]
        report: bool,
    },

    /// Print aggregated token usage and estimated cost
    Report {
        /// Filter to recent activity, e.g. 24h, 7d, 4w
        #[arg(long)]
        since: Option<String>,
        /// Group rows by: model | provider | sandbox
        #[arg(long, default_value = "model")]
        by: String,
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },

    /// Show the effective pricing table (bundled + user overrides)
    Pricing,
}

#[derive(Subcommand)]
pub enum ReportAction {
    /// Register a new task in the database (called by wrappers at agent startup)
    Start {
        /// Task ID (UUID)
        task_id: String,
        /// Agent type (claude_code, pi, etc.)
        agent_type: String,
        /// Working directory
        cwd: String,
        /// Task title/description
        title: String,
        /// Process ID
        #[arg(long)]
        pid: Option<i32>,
        /// Parent process ID
        #[arg(long)]
        ppid: Option<i32>,
        /// Zellij pane ID
        #[arg(long)]
        zellij_pane_id: Option<u32>,
        /// Zellij session name (needed to target panes from outside the session)
        #[arg(long)]
        zellij_session: Option<String>,
        /// Session ID (if already known at startup)
        #[arg(long)]
        session_id: Option<String>,
    },

    /// Store the agent session ID so the next attach can resume it
    ///
    /// Called by the Claude Stop hook.
    #[command(name = "session-id")]
    SessionId {
        /// Task ID
        task_id: String,
        /// Agent session ID (Claude UUID)
        session_id: String,
    },

    /// Store the pi-family session file path so the next attach can resume it
    ///
    /// Called by the pi/omp nibble-memory extension at session start. This is
    /// the authoritative task→session mapping: without it, attach falls back
    /// to guessing the newest session file for the repo, which is ambiguous
    /// once more than one session exists (main + --btw side sessions).
    #[command(name = "session-path")]
    SessionPath {
        /// Task ID
        task_id: String,
        /// Absolute path to the session JSONL file (container or host path)
        path: String,
    },

    /// Record the zellij pane hosting an agent (for sidebar jumps / goto)
    ///
    /// Called by the pi/omp nibble-memory extension with ZELLIJ_PANE_ID so
    /// bare-launched sessions (no wrapper AGENT_TASK_ID env) can still be
    /// focused from the sidebar's 1-9 keys or `nibble goto`.
    #[command(name = "pane-id")]
    PaneId {
        /// Task ID
        task_id: String,
        /// Zellij pane ID
        pane_id: u32,
        /// Agent process ID — lets the sidebar's liveness reconcile mark
        /// the row exited when a killed agent leaves its pane open
        #[arg(long)]
        pid: Option<i32>,
    },

    /// Transition a task's live status (called by agent hooks/extensions)
    ///
    /// States: running (actively generating), blocked (needs user input —
    /// permission prompt, question), completed (turn done, idle), exited.
    Status {
        /// Task ID
        task_id: String,
        /// New state: running | blocked | completed | exited
        state: String,
        /// Short context for blocked (e.g. the permission prompt summary)
        #[arg(short, long)]
        message: Option<String>,
    },

    /// Record what a task is about, shown as its sidebar topic line
    ///
    /// Called by agent hooks with each user prompt. Only the first
    /// substantive prompt is kept (slash commands are skipped) unless
    /// `--replace` is given; `--replace` with empty text clears the topic.
    Topic {
        /// Task ID
        task_id: String,
        /// Prompt or summary text (first line is kept, whitespace collapsed)
        text: String,
        /// Overwrite an existing topic instead of keeping the first one
        #[arg(long)]
        replace: bool,
    },
}

/// Agent-selection flags shared by `sandbox spawn` and `sandbox attach`.
#[derive(clap::Args)]
pub struct AgentFlags {
    /// Start a new session (generates a fresh random UUID, replacing the stored one)
    #[arg(long)]
    pub fresh: bool,
    /// Create a git worktree for this branch and spawn a sandbox for it.
    /// The worktree is created at <repo_parent>/<repo_name>--<branch-slug>.
    /// The branch is auto-created from the repo's current HEAD if it doesn't exist.
    #[arg(long)]
    pub branch: Option<String>,
    /// Use Hermes Agent instead of Claude Code.
    /// Spawns a dedicated Hermes container with gateway support.
    #[arg(long)]
    pub hermes: bool,
    /// Use the upstream pi coding agent (@earendil-works/pi-coding-agent)
    /// instead of Claude Code. Installs pi plus any [pi].extensions
    /// (e.g. pi-dynamic-workflows) at spawn time.
    #[arg(long, conflicts_with = "omp")]
    pub pi: bool,
    /// Use the omp (oh-my-pi) coding agent instead of Claude Code.
    /// Installs the standalone omp binary plus any [pi].extensions at spawn time.
    #[arg(long)]
    pub omp: bool,
}
#[derive(Subcommand)]
pub enum SandboxAction {
    /// Spawn a sandboxed agent for a repo
    Spawn {
        /// Path to the repository to run the agent in
        repo_path: String,
        /// Task description
        #[arg(short, long)]
        task: Option<String>,
        /// Sandbox image to use
        #[arg(long, default_value = "nibble-sandbox:latest")]
        image: String,
        /// Use a specific Claude session UUID instead of the deterministic repo UUID
        #[arg(long)]
        session_id: Option<String>,
        #[command(flatten)]
        agent: AgentFlags,
    },

    /// List all sandbox containers and their status
    List,

    /// Attach to a running sandbox container with an interactive bash shell
    Bash {
        /// Repo path (e.g. "." or "/path/to/repo") OR container name
        container_or_path: String,
    },

    /// Attach to a running sandbox container
    Attach {
        /// Repo path (e.g. "." or "/path/to/repo") OR container name
        container_or_path: String,
        /// Start an independent side session that doesn't overwrite which session the main
        /// attach would continue. The session is kept on disk like any other.
        /// Useful for ad-hoc research or non-conflicting changes alongside a main session.
        #[arg(long)]
        btw: bool,
        /// Resume a specific session by ID (from `nibble session list`).
        /// Overrides the stored session for this task.
        #[arg(long)]
        session: Option<String>,
        #[command(flatten)]
        agent: AgentFlags,
    },

    /// Stop and remove a sandbox container
    Kill {
        /// Repo path (e.g. "." or "/path/to/repo") OR container name. Omit when --all is set.
        container_or_path: Option<String>,
        /// Kill all running sandbox containers
        #[arg(long)]
        all: bool,
        /// Also remove the git worktree associated with this sandbox (if any).
        /// Warns and prompts if the worktree has uncommitted changes, unless --force is set.
        #[arg(long)]
        worktree: bool,
        /// Skip the dirty-worktree confirmation prompt and remove immediately (implies --worktree).
        #[arg(short, long)]
        force: bool,
        /// Derive the worktree path from this branch name and use it as the kill target.
        /// Equivalent to: nibble sandbox kill <repo>--<branch-slug> --worktree
        #[arg(long)]
        branch: Option<String>,
    },
    /// Resume sandboxes after a host reboot
    Resume {
        #[arg(short, long)]
        all: bool,
    },

    /// Build or rebuild the sandbox base image (called by install.sh — use ./install.sh --rebuild)
    #[command(hide = true)]
    Build {
        #[arg(long, default_value = "nibble-sandbox:latest")]
        image: String,
        #[arg(long)]
        rebuild: bool,
    },
}

#[derive(Subcommand)]
pub enum HermesAction {
    /// Spawn the Hermes Agent sandbox (gateway as PID 1, no primary repo)
    Init,

    /// Attach to the Hermes CLI inside the sandbox (auto-spawns if needed)
    Attach {
        /// Start a fresh hermes session instead of resuming
        #[arg(long)]
        fresh: bool,
    },

    /// Mount a repo directory into the Hermes sandbox (restarts container)
    Mount {
        /// Path to the repo directory to mount
        repo_path: String,
        /// Custom mount name inside /repos/ (defaults to directory basename)
        #[arg(long)]
        name: Option<String>,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Unmount a repo from the Hermes sandbox (restarts container)
    Unmount {
        /// Path to the repo directory to unmount
        repo_path: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Show Hermes sandbox status and mounted repos
    List,

    /// Stop and remove the Hermes sandbox (repos are preserved for next init)
    Kill,
}

#[derive(Subcommand)]
pub enum MemoryAction {
    /// Search memories by keyword
    Search {
        /// Search query
        query: String,
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
        /// Filter by memory type
        #[arg(long)]
        r#type: Option<String>,
        /// Maximum results to return
        #[arg(short, long, default_value = "20")]
        limit: usize,
        /// Use semantic search (Phase 3: falls back to keyword for now)
        #[arg(long)]
        semantic: bool,
    },

    /// List all memories
    List {
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
        /// Filter by memory type
        #[arg(long)]
        r#type: Option<String>,
        /// Only show memories since this date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Maximum results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Show full content of a specific memory
    Show {
        /// Memory ID (or prefix)
        id: String,
        /// Also display the full session transcript
        #[arg(long)]
        with_session: bool,
    },

    /// Write a new memory or update an existing one
    Write {
        /// Memory content
        content: String,
        /// Memory type: decision, pattern, user_instruction, observation, bug_record, session_summary
        #[arg(short, long, default_value = "observation")]
        r#type: String,
        /// Project name (defaults to current directory name)
        #[arg(short, long)]
        project: Option<String>,
        /// Comma-separated tags
        #[arg(short, long)]
        tags: Option<String>,
        /// Update an existing memory by ID
        #[arg(long)]
        update: Option<String>,
        /// Short title for quick identification (recommended for session_summary)
        #[arg(short, long)]
        title: Option<String>,
    },

    /// Show memories linked to a specific session
    BySession {
        /// Session ID (or prefix)
        session_id: String,
    },

    /// Load relevant memories and lessons as a context briefing
    Context {
        /// Query describing what you're working on
        query: String,
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
        /// Maximum results to return
        #[arg(short, long, default_value = "5")]
        limit: usize,
    },

    /// Delete a memory
    Forget {
        /// Memory ID to delete
        id: String,
    },

    /// Show memory statistics
    Stats {
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
    },

    /// Capture a session event (internal: called by hooks/extensions)
    #[command(hide = true)]
    Capture {
        /// Task ID
        task_id: String,
        /// Event role: user, assistant, tool, system
        role: String,
        /// Event content
        content: String,
        /// Tool name (for tool events)
        #[arg(long)]
        tool_name: Option<String>,
        /// Tool input (for tool events)
        #[arg(long)]
        tool_input: Option<String>,
        /// Tool output (for tool events)
        #[arg(long)]
        tool_output: Option<String>,
    },

    /// List and search lessons
    Lessons {
        /// Context description for semantic matching of relevant lessons
        #[arg(long)]
        context: Option<String>,
        /// Filter by status: active, resolved, encoded
        #[arg(long, default_value = "active")]
        status: String,
        /// Filter by severity: low, medium, high, critical
        #[arg(long)]
        severity: Option<String>,
        /// Maximum results to return
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },

    /// Add a new lesson
    LessonAdd {
        /// Lesson content
        content: String,
        /// Category: spec_gap, impl_bug, test_gap, audit_blind_spot, qa_catch, process
        #[arg(short, long, default_value = "impl_bug")]
        category: String,
        /// Severity: low, medium, high, critical
        #[arg(short, long, default_value = "medium")]
        severity: String,
        /// How to prevent this issue
        #[arg(short, long, default_value = "")]
        prevention: String,
        /// Project name
        #[arg(short, long)]
        project: Option<String>,
        /// Comma-separated tags
        #[arg(short, long)]
        tags: Option<String>,
    },

    /// Resolve a lesson
    LessonResolve {
        /// Lesson ID
        id: String,
        /// Resolution note
        #[arg(short, long)]
        note: Option<String>,
    },

    /// Browse memories interactively (opens in pager)
    Inspect {
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
    },

    /// Remove duplicate session_summary memories (keep the newest)
    Dedup {
        /// Actually delete duplicates (default: dry-run)
        #[arg(long)]
        yes: bool,
    },

    /// Rebuild the index cache and regenerate index.md
    Reindex,

    /// Display memory system configuration and status
    Config {
        /// Launch interactive setup wizard to configure memory settings
        #[arg(long)]
        setup: bool,
    },

    /// Sync memory store (git add + commit + pull + push)
    Sync,

    /// Copy the original agent session file into the memory repo as a standalone archive
    Archive {
        /// Task ID of the session to archive
        task_id: String,
    },

    /// Extract memories and lessons from a captured session
    Summarize {
        /// Task ID of the session to summarize
        task_id: String,
        /// Force re-summarization even if already done
        #[arg(long)]
        force: bool,
        /// Path to a pi session JSONL file to summarize instead of capture JSONL
        #[arg(long)]
        from_pi_session: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// List all discoverable sessions across agents (browser-history style)
    List {
        /// Filter by agent type: claude, pi, hermes
        #[arg(short, long)]
        agent: Option<String>,
        /// Filter by repo: paths (".", "~", relative) resolve like other commands
        /// and match the repo plus its subdirectories; plain strings match
        /// workspace substrings
        #[arg(short, long)]
        repo: Option<String>,
        /// Filter to sessions for a specific sandbox repo (same path resolution as attach)
        #[arg(long)]
        sandbox: Option<String>,
        /// Show only sessions from today
        #[arg(long, group = "date_filter")]
        today: bool,
        /// Show only sessions from yesterday
        #[arg(long, group = "date_filter")]
        yesterday: bool,
        /// Show only sessions from the last 7 days
        #[arg(long, group = "date_filter")]
        week: bool,
        /// Show only sessions from the last 30 days
        #[arg(long, group = "date_filter")]
        month: bool,
        /// Show only the last N sessions
        #[arg(long)]
        last: Option<usize>,
        /// Maximum results to show per group
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Show full details for one session: full ID, transcript path, resume command
    Info {
        /// Session ID or unambiguous prefix (from `nibble session list`)
        id: String,
    },

    /// Read and display a session transcript by its ID
    Read {
        /// Session ID (from `nibble session list`)
        id: String,
        /// Output raw JSON/JSONL instead of formatted transcript
        #[arg(long)]
        raw: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum CronAction {
    /// Register a repo on this machine and review its job files
    /// (`.nibble/cron/*.md`): only approved ones run here, on schedule once
    /// the scheduler service is installed. With --schedule and --prompt, or
    /// --file, first writes a new job file into the repo.
    Add {
        /// Repository (default: the git repo around the current directory)
        #[arg(short, long)]
        repo: Option<String>,

        /// Cron schedule for a new job (e.g. "0 9 * * 1-5" for 9am weekdays)
        #[arg(short, long)]
        schedule: Option<String>,

        /// Prompt for a new job
        #[arg(short, long)]
        prompt: Option<String>,

        /// Markdown job definition to copy into the repo
        #[arg(short, long)]
        file: Option<String>,

        /// Title of the new job; its file name is made from it
        #[arg(short, long)]
        label: Option<String>,

        /// Expiry for the new job: RFC3339 ("2026-04-01T00:00:00Z") or
        /// relative ("+7d"). The job stops running after it.
        #[arg(long)]
        expires: Option<String>,

        /// Approve the repo's job files without asking
        #[arg(long)]
        yes: bool,
    },

    /// List the jobs of registered repos (optionally one repo)
    List {
        /// Only this repo
        repo_path: Option<String>,
    },

    /// Change a job file: with flags, or in $EDITOR when none are given
    Edit {
        /// Job name, repo/name, or path of the job file
        id: String,

        /// New schedule expression
        #[arg(short, long)]
        schedule: Option<String>,

        /// New prompt text
        #[arg(short, long)]
        prompt: Option<String>,

        /// New title
        #[arg(short, long)]
        label: Option<String>,

        /// Set enabled = true in the file (every machine)
        #[arg(long)]
        enable: bool,

        /// Set enabled = false in the file (every machine)
        #[arg(long)]
        disable: bool,

        /// Expiry: RFC3339, relative ("+7d"), or "none" to remove it
        #[arg(long)]
        expires: Option<String>,
    },

    /// Review and approve job files to run on this machine (a file that
    /// changes needs approving again)
    Approve {
        /// Job name, repo/name, or path (default: every pending job of --repo)
        job: Option<String>,
        /// Repository (default: the git repo around the current directory)
        #[arg(short, long)]
        repo: Option<String>,
        /// Approve without asking
        #[arg(long)]
        yes: bool,
    },

    /// Pause a job on this machine only (the file is untouched)
    Stop {
        /// Job name, repo/name, or path of the job file
        id: String,
    },

    /// Resume a job paused with stop
    Start {
        /// Job name, repo/name, or path of the job file
        id: String,
    },

    /// Delete a job file
    Kill {
        /// Job name, repo/name, or path of the job file
        id: String,
    },

    /// Run a job now
    Run {
        /// Job name, repo/name, or path of the job file
        id: String,
    },

    /// Stop running a repo's jobs on this machine (its files are kept)
    Forget {
        /// Repository path
        repo: String,
    },

    /// Move jobs an older nibble kept in ~/.nibble/tasks.db into their repos,
    /// after showing what would happen
    Import {
        /// Import without asking
        #[arg(long)]
        yes: bool,
    },

    /// Values jobs get as environment variables (`env = [...]` in the file),
    /// kept per repo outside every sandbox
    Secret {
        #[command(subcommand)]
        action: CronSecretAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum CronSecretAction {
    /// Store a value (read without echo, or from stdin)
    Set {
        /// Variable name, e.g. NORTHFLANK_API_TOKEN
        name: String,
        /// Repository (default: the git repo around the current directory)
        #[arg(short, long)]
        repo: Option<String>,
    },
    /// List stored names (never values)
    List {
        /// Only this repository
        #[arg(short, long)]
        repo: Option<String>,
    },
    /// Remove a value
    Unset {
        /// Variable name
        name: String,
        /// Repository (default: the git repo around the current directory)
        #[arg(short, long)]
        repo: Option<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum LmAction {
    /// List all .gguf model files found in configured model directories
    List,

    /// Switch the active model and restart llama-server
    ///
    /// Accepts a partial model name (e.g. "gemma", "Qwen3").
    /// Sampling parameters are read from profiles.toml in the model directory.
    Use {
        /// Partial or full filename of the .gguf model to activate
        model: String,
    },
}
