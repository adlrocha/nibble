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
}
