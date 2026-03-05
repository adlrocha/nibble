use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-inbox")]
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

    // ── Internal sandbox subcommands (called by agent-sandbox script) ──────────

    /// [internal] Spawn a sandboxed agent
    #[command(hide = true, name = "_sandbox_spawn")]
    SandboxSpawn {
        repo_path: String,
        #[arg(short, long)]
        task: Option<String>,
        #[arg(long, default_value = "agent-inbox-sandbox:latest")]
        image: String,
        /// Always start a fresh Claude session (no --continue / --resume)
        #[arg(long)]
        fresh: bool,
        /// Resume a specific Claude session ID instead of auto-detecting
        #[arg(long)]
        session_id: Option<String>,
    },

    /// [internal] List sandboxes
    #[command(hide = true, name = "_sandbox_list")]
    SandboxList,

    /// [internal] Attach to a sandbox container
    #[command(hide = true, name = "_sandbox_attach")]
    SandboxAttach {
        task_id: String,
        /// Start a fresh session instead of resuming the last conversation
        #[arg(long)]
        fresh: bool,
        /// Use Kimi as the LLM backend (reads KIMI_BASE_URL and KIMI_API_KEY from host env)
        #[arg(long)]
        kimi: bool,
    },

    /// [internal] Kill a sandbox
    #[command(hide = true, name = "_sandbox_kill")]
    SandboxKill {
        /// Task ID to kill (omit when --all is set)
        task_id: Option<String>,
        /// Kill all running sandbox containers
        #[arg(long)]
        all: bool,
    },

    /// Restart all stopped sandbox containers (e.g. after a host reboot)
    ///
    /// Attempts to start any stopped containers tracked in the database.
    /// Containers that no longer exist are cleaned up.
    #[command(name = "sandbox-restart")]
    SandboxRestart,

    /// [internal] Resume sandboxes after reboot
    #[command(hide = true, name = "_sandbox_resume")]
    SandboxResume {
        #[arg(short, long)]
        all: bool,
    },

    /// [internal] Build the sandbox image
    #[command(hide = true, name = "_sandbox_build")]
    SandboxBuild {
        #[arg(long, default_value = "agent-inbox-sandbox:latest")]
        image: String,
        #[arg(long)]
        rebuild: bool,
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
