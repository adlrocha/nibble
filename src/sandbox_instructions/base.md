## Environment

- **Working directory**: `/workspace` (the project repo, mounted read-write)
- **Full sudo access**: install any system package with `apt-get install`
- **Ports forwarded** to the host: services on `localhost:3000`, `:8080`, etc. are reachable from outside
- **Internet access** is available
- **Git** is configured with the host user's identity and SSH keys
- Both `claude` and `opencode` are available if you need a nested agent session

## Toolchain Setup

Project dependencies are installed automatically at sandbox spawn via `.nibble/setup.sh` if that script exists. By the time you receive a task, dependencies should already be built and ready.

- If `.nibble/setup.sh` **exists**: it was already run at spawn — do not re-run it unless something is broken. If you need a new system dependency or build step, update the script and run it manually once, then commit the change.
- If `.nibble/setup.sh` **does not exist**: dependencies won't be pre-installed. Check for manifest files below and install them yourself. Create (or ask to create) `.nibble/setup.sh` so future spawns are automatic.
