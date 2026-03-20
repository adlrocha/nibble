use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nibble")]
#[command(about = "Track and monitor tasks across multiple LLM/coding agents", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all tasks (default shows only tasks needing attention)
    List {
        /// Show all tasks regardless of status
        #[arg(short, long)]
        all: bool,

        /// Filter by status: running, completed, exited
        #[arg(short, long)]
        status: Option<String>,
    },

    /// Show detailed information about a specific task
    Show {
        /// Task ID to show
        task_id: String,
    },

    /// Clear/archive a task
    Clear {
        /// Task ID to clear
        task_id: String,
    },

    /// Clear all completed and exited tasks
    ClearAll,

    /// Force clear ALL tasks regardless of status (use when stuck)
    Reset {
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Watch tasks in real-time (refreshes every 2 seconds)
    Watch,

    /// Manually trigger cleanup of old completed tasks
    Cleanup {
        /// Retention period in seconds (default: 3600)
        #[arg(short, long, default_value = "3600")]
        retention_secs: i64,
    },

    /// Mark Running tasks as Exited when their process is no longer alive
    ///
    /// Also checks sandbox containers: removes DB state for containers that are
    /// no longer running (caught a crash or host reboot).
    /// Called automatically by `listen`; run manually to repair a stuck dashboard.
    Prune,

    /// Report task status (internal command used by wrappers)
    Report {
        #[command(subcommand)]
        action: ReportAction,
    },

    /// Monitor a process for completion or attention needs (internal command)
    Monitor {
        /// Task ID to monitor
        task_id: String,

        /// Process ID to monitor
        pid: i32,
    },

    // ── Sandbox subcommands ────────────────────────────────────────────────────

    /// Manage sandboxed agent containers
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },

    /// Inject a message into a running sandbox agent (bypasses Telegram)
    Inject {
        /// Task ID of the agent to inject into
        task_id: String,
        /// Message to send
        message: String,
    },

    /// Run the Telegram long-polling daemon (routes phone replies back to agents)
    Listen,

    /// Send a Telegram notification (used by hooks and wrappers)
    Notify {
        /// Message body to send (agent last output or permission request)
        #[arg(short, long)]
        message: String,

        /// Optional task ID to attach context (agent type, title, elapsed time)
        #[arg(short, long)]
        task_id: Option<String>,

        /// Mark this as an attention-required notification (permission request, question, etc.)
        /// Uses a distinct visual style so it stands out from regular completion notifications.
        #[arg(long)]
        attention: bool,
    },

    /// Manage scheduled cron jobs for sandboxes
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
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
        /// Start a new session (generates a fresh random UUID, replacing the stored one)
        #[arg(long)]
        fresh: bool,
        /// Use a specific Claude session UUID instead of the deterministic repo UUID
        #[arg(long)]
        session_id: Option<String>,
    },

    /// List all sandbox containers and their status
    List,

    /// Attach to a running sandbox container
    Attach {
        /// Task ID (or a prefix) OR a repo path (e.g. "." or "/path/to/repo")
        task_id_or_path: String,
        /// Start a fresh session instead of resuming the last conversation
        #[arg(long)]
        fresh: bool,
        /// Use Kimi as the LLM backend (reads KIMI_BASE_URL and KIMI_API_KEY from host env)
        #[arg(long)]
        kimi: bool,
        /// Use GLM as the LLM backend (reads GLM_BASE_URL and GLM_API_KEY from host env)
        #[arg(long)]
        glm: bool,
    },

    /// Stop and remove a sandbox container
    Kill {
        /// Task ID (or prefix) OR repo path (e.g. "." or "/path/to/repo"). Omit when --all is set.
        task_id_or_path: Option<String>,
        /// Kill all running sandbox containers
        #[arg(long)]
        all: bool,
    },

    /// Restart all stopped sandbox containers (e.g. after a host reboot)
    ///
    /// Attempts to start any stopped containers tracked in the database.
    /// Containers that no longer exist are cleaned up.
    Restart,

    /// Resume sandboxes after a host reboot
    Resume {
        #[arg(short, long)]
        all: bool,
    },

    /// Build or rebuild the sandbox base image
    Build {
        /// Image name/tag to build
        #[arg(long, default_value = "nibble-sandbox:latest")]
        image: String,
        /// Force a clean rebuild from scratch
        #[arg(long)]
        rebuild: bool,
    },

    /// Delete old Claude conversation files for a sandbox to free memory
    ///
    /// Removes .jsonl conversation files from ~/.claude/projects/ that belong
    /// to this sandbox's repo. Keeps the most recent session intact.
    /// Use before `attach --fresh` to start completely clean.
    Gc {
        /// Task ID (or prefix) OR repo path (e.g. "." or "/path/to/repo")
        task_id_or_path: String,
        /// Also delete the most recent session (full wipe, no resume possible)
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
pub enum ReportAction {
    /// Report task start
    Start {
        /// Task ID (UUID)
        task_id: String,

        /// Agent type (claude_code, opencode, etc.)
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

        /// Zellij pane ID (set automatically when running inside zellij)
        #[arg(long)]
        zellij_pane_id: Option<u32>,

        /// Claude Code session ID (set by the Stop hook for resume support)
        #[arg(long)]
        session_id: Option<String>,
    },

    /// Report task completion
    Complete {
        /// Task ID
        task_id: String,

        /// Exit code
        #[arg(long)]
        exit_code: Option<i32>,
    },

    /// Report task is running (generating)
    Running {
        /// Task ID
        task_id: String,
    },

    /// Report task has exited (process terminated)
    Exited {
        /// Task ID
        task_id: String,

        /// Exit code
        #[arg(long)]
        exit_code: Option<i32>,
    },

    /// Store the last assistant message on the task (called by the Stop hook before SessionEnd)
    LastMessage {
        /// Task ID
        task_id: String,

        /// The last assistant message text
        message: String,
    },

    /// Update the Claude session ID for an existing task (called by the Stop hook)
    SessionId {
        /// Task ID
        task_id: String,

        /// Claude Code session ID from the hook JSON
        session_id: String,
    },
}

#[derive(Subcommand)]
pub enum CronAction {
    /// Add a new cron job targeting a repo path.
    /// At trigger time nibble will find or spawn a sandbox for that repo automatically.
    Add {
        /// Path to the repository this cron job targets.
        /// If omitted, repo_path must be set in the --file markdown.
        #[arg(short, long)]
        repo: Option<String>,

        /// Cron schedule expression (e.g., "0 9 * * 1-5" for 9am weekdays)
        #[arg(short, long)]
        schedule: Option<String>,

        /// Prompt text to send (alternative to --file)
        #[arg(short, long)]
        prompt: Option<String>,

        /// Path to markdown file with cron definition
        #[arg(short, long)]
        file: Option<String>,

        /// Label/name for this cron job
        #[arg(short, long)]
        label: Option<String>,

        /// Expiry datetime in RFC3339 format (e.g. "2026-04-01T00:00:00Z").
        /// Job is auto-disabled after this time.
        #[arg(long)]
        expires: Option<String>,
    },

    /// List cron jobs (optionally filtered by repo path)
    List {
        /// Optional canonical repo path to filter by
        repo_path: Option<String>,
    },

    /// Edit an existing cron job
    Edit {
        /// Cron job ID or label
        id: String,

        /// New schedule expression
        #[arg(short, long)]
        schedule: Option<String>,

        /// New prompt text
        #[arg(short, long)]
        prompt: Option<String>,

        /// New label
        #[arg(short, long)]
        label: Option<String>,

        /// Enable the cron job
        #[arg(long)]
        enable: bool,

        /// Disable the cron job
        #[arg(long)]
        disable: bool,

        /// Set or update expiry datetime in RFC3339 format (e.g. "2026-04-01T00:00:00Z").
        /// Pass "none" to remove an existing expiry.
        #[arg(long)]
        expires: Option<String>,
    },

    /// Delete a cron job
    Del {
        /// Cron job ID or label
        id: String,
    },

    /// Run a cron job immediately (for testing)
    Run {
        /// Cron job ID or label
        id: String,
    },
}
