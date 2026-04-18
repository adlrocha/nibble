use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nibble")]
#[command(about = "Manage sandboxed coding agents and scheduled tasks", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
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

    /// Report task status (internal command used by wrappers and hooks)
    Report {
        #[command(subcommand)]
        action: ReportAction,
    },
}

#[derive(Subcommand)]
pub enum ReportAction {
    /// Register a new task in the database (called by wrappers at agent startup)
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
        /// Zellij pane ID
        #[arg(long)]
        zellij_pane_id: Option<u32>,
        /// Session ID (if already known at startup)
        #[arg(long)]
        session_id: Option<String>,
    },

    /// Store the agent session ID so the next attach can resume it
    ///
    /// Called by the Claude Stop hook and the opencode post-exit epilogue.
    #[command(name = "session-id")]
    SessionId {
        /// Task ID
        task_id: String,
        /// Agent session ID (Claude UUID or opencode ses_... ID)
        session_id: String,
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
        /// Create a git worktree for this branch and spawn a sandbox for it.
        /// The worktree is created at <repo_parent>/<repo_name>--<branch-slug>.
        /// The branch is auto-created from the repo's current HEAD if it doesn't exist.
        #[arg(long)]
        branch: Option<String>,
        /// Enable the AI Factory development pipeline (spec → implement → TDD → adversarial → risk → QA).
        /// Default is controlled by factory.enabled in ~/.nibble/config.toml.
        #[arg(long)]
        factory: Option<bool>,
        /// Use Hermes Agent instead of Claude Code / OpenCode.
        /// Spawns a dedicated Hermes container with gateway support.
        #[arg(long)]
        hermes: bool,
        /// Use Pi (pi.dev) coding agent instead of Claude Code.
        /// Installs @mariozechner/pi-coding-agent at spawn time.
        #[arg(long)]
        pi: bool,
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
        /// Start a fresh session instead of resuming the last conversation
        #[arg(long)]
        fresh: bool,
        /// Start an independent throwaway session that doesn't affect the main session history.
        /// Useful for ad-hoc research or non-conflicting changes alongside a main session.
        #[arg(long)]
        btw: bool,
        /// Use Kimi as the LLM backend (reads KIMI_BASE_URL and KIMI_API_KEY from host env)
        #[arg(long)]
        kimi: bool,
        /// Use GLM as the LLM backend (reads GLM_BASE_URL and GLM_API_KEY from host env)
        #[arg(long)]
        glm: bool,
        /// Use opencode instead of Claude Code as the coding agent
        #[arg(long)]
        opencode: bool,
        /// Use Hermes Agent instead of Claude Code / OpenCode
        #[arg(long)]
        hermes: bool,
        /// Use Pi (pi.dev) coding agent instead of Claude Code
        #[arg(long)]
        pi: bool,
        /// Create a git worktree for this branch and spawn+attach a sandbox for it.
        /// The worktree is created at <repo_parent>/<repo_name>--<branch-slug>.
        /// The branch is auto-created from the repo's current HEAD if it doesn't exist.
        #[arg(long)]
        branch: Option<String>,
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

    /// Build or rebuild the sandbox base image (called by install.sh — use ./install.sh --rebuild)
    #[command(hide = true)]
    Build {
        #[arg(long, default_value = "nibble-sandbox:latest")]
        image: String,
        #[arg(long)]
        rebuild: bool,
    },

    /// Delete old Claude conversation files for a sandbox to free memory
    ///
    /// Removes .jsonl conversation files from ~/.claude/projects/ that belong
    /// to this sandbox's repo. Keeps the most recent session intact.
    /// Use before `attach --fresh` to start completely clean.
    Gc {
        /// Repo path (e.g. "." or "/path/to/repo") OR container name
        container_or_path: String,
        /// Also delete the most recent session (full wipe, no resume possible)
        #[arg(long)]
        all: bool,
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
    Kill {
        /// Cron job ID or label
        id: String,
    },

    /// Run a cron job immediately (for testing)
    Run {
        /// Cron job ID or label
        id: String,
    },
}
