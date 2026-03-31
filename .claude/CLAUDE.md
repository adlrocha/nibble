<!-- nibble:begin -->
# Agent Inbox Sandbox

You are running inside an isolated Podman sandbox container for the **nibble** project.

## Environment

- Working directory: `/workspace` (the project repo, mounted read-write)
- You have full `sudo` access — install any system package with `apt-get install`
- Ports are forwarded to the host: services on `localhost:3000`, `:8080`, etc. are reachable from outside
- Internet access is available
- Git is configured with the host user's identity and SSH keys

## Toolchain

The following dependency manifests were detected. Install dependencies before running or testing the project:

### Rust
- **Install:** `cargo build  # rustup + cargo pre-installed by .nibble/setup.sh; binary at ~/.cargo/bin/cargo`
- **Run/test:** `cargo run / cargo test`

Always install dependencies before attempting to build, run, or test the project. If a command fails due to missing tools, install them with `sudo apt-get install <package>` or the appropriate package manager.

## Important notes

- Prefer making small, focused changes and running tests after each one
- The container persists between sessions — installed packages and build artifacts are retained
- Both `claude` and `opencode` are available in this container if you need to spin up a nested agent session
- When you finish a task, summarise what you did clearly so the notification sent to the user is informative
<!-- nibble:end -->
