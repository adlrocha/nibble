# Blueprint: Hermes Agent Sandbox Support

## Summary

Add first-class support for running Hermes Agent (NousResearch's open-source coding agent)
inside nibble's Podman sandboxes. A single sandbox runs both the Hermes gateway (long-running
daemon for Telegram/Discord/cron) and the Hermes CLI, with multiple repos bind-mounted and
access to a local LLM on the host via `--network host`. This enables sandboxed, always-on
Hermes usage for day-to-day work.

## Scope

### In scope
- `AgentType::Hermes` variant in the agent type system
- Dockerfile generation that installs Hermes Agent (Python 3.11 + uv + hermes-agent)
- Volume mounts: `~/.hermes/` config dir, multiple repos, cache dirs
- Hermes-specific spawn: starts `hermes gateway` as the main process (instead of `sleep infinity`)
- Hermes-specific attach: opens `hermes --continue` or fresh `hermes` CLI session
- Config extension: `[hermes]` section with `repos` list and optional settings
- CLI flags: `--hermes` on `sandbox spawn` and `sandbox attach`
- Inject support: send messages to Hermes via `hermes --continue --message` or stdin

### Out of scope
- Multi-sandbox Hermes isolation (parallel instances with separate configs) -- future work
- Hermes profile support (`--profile`) -- future work
- Running Hermes gateway on the host outside a sandbox
- Modifying Hermes Agent itself

### Dependencies
- Hermes Agent installer: `curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash`
- Python 3.11 (installed by Hermes installer via uv)
- Host must have `~/.hermes/config.yaml` configured (LLM provider pointing to `localhost:6969` or similar)

## Interfaces

### Public API / Exports

| Name | Input | Output | Errors |
|------|-------|--------|--------|
| `AgentType::Hermes` | -- | variant | -- |
| `PodmanSandbox::generate_hermes_dockerfile()` | -- | `String` (Dockerfile) | -- |
| `PodmanSandbox::spawn()` (extended) | `task_id, repo_path, config` with Hermes flag | `ContainerInfo` | spawn failure, image build failure |
| `cmd_sandbox_attach()` (extended) | `--hermes` flag | exec into Hermes CLI | container not found, Hermes not installed |
| `HermesConfig` struct | -- | parsed from `config.toml` | missing config |
| `Config.hermes` field | -- | `HermesConfig` | -- |

### Data Types

```rust
// Added to AgentType enum in src/models/task.rs
AgentType::Hermes  // serializes as "hermes"

// New config struct in src/config.rs
struct HermesConfig {
    /// Repo paths to mount into the sandbox (mounted at /repos/<dirname>)
    repos: Vec<String>,       // default: []
    /// Whether to start the gateway as the main process
    gateway: bool,            // default: true
    /// Hermes image name (separate from nibble-sandbox)
    image: String,            // default: "nibble-hermes:latest"
}
```

### Events / Side Effects

- **spawn**: Builds `nibble-hermes:latest` image if missing (includes Python + Hermes)
- **spawn**: Starts `hermes gateway` as PID 1 (if `gateway: true`) or `sleep infinity`
- **spawn**: Mounts all configured repos under `/repos/`
- **spawn**: Mounts `~/.hermes/` -> `/home/node/.hermes/` (config, sessions, memories)
- **attach**: Exec into container running `hermes --continue` or fresh `hermes`
- **kill**: Stops gateway gracefully (SIGTERM -> Hermes handles cleanup)

## State Machine / Flow

```
[No Container] ── nibble sandbox spawn --hermes ──> [Building Image]
[Building Image] ── image ready ──> [Starting Container]
[Starting Container] ── container up ──> [Gateway Running]
[Gateway Running] ── nibble sandbox attach --hermes ──> [CLI Session Active]
[CLI Session Active] ── user exits CLI ──> [Gateway Running]
[Gateway Running] ── nibble sandbox kill ──> [No Container]
[Gateway Running] ── host reboot ──> [Container Stopped]
[Container Stopped] ── nibble sandbox resume ──> [Gateway Running]
```

## Invariants

1. (INV-1) The Hermes gateway process is always the main process (PID 1 via exec or
   entrypoint) when `gateway: true`, ensuring the container stops if the gateway crashes.
2. (INV-2) `~/.hermes/` on the host is always mounted read-write so Hermes can persist
   sessions, memories, and config changes across container restarts.
3. (INV-3) All repos listed in `hermes.repos` config are mounted read-write under
   `/repos/<basename>` with unique names (collision-handling if two repos share a basename).
4. (INV-4) `--network host` is always used so Hermes can reach the local LLM at
   `localhost:6969` (or any other host port).
5. (INV-5) Only one Hermes sandbox can exist at a time (enforced at spawn time). Future
   work will lift this restriction.
6. (INV-6) The Hermes Dockerfile is separate from the standard nibble-sandbox Dockerfile
   to avoid bloating the Claude/OpenCode image with Python dependencies.

## Error Handling Strategy

| Error | Type | Handling |
|-------|------|----------|
| `~/.hermes/` doesn't exist on host | Expected | Create it with minimal structure, warn user to run `hermes setup` |
| Hermes install fails in Dockerfile | Expected | Fail image build with clear error message |
| Gateway crashes inside container | Expected | Container stops (PID 1 exit). User runs `nibble sandbox resume` |
| Repo path in config doesn't exist | Expected | Skip with warning, mount only existing repos |
| Duplicate repo basenames | Expected | Append `-1`, `-2` suffixes to mount points |
| Container already exists for Hermes | Expected | Reuse existing container (same as current nibble behavior) |
| `hermes` binary not found in container | Unexpected | Error on attach with instructions to rebuild image |

## Acceptance Criteria

1. (AC-1) `nibble sandbox spawn --hermes` creates a Podman container with Hermes Agent
   installed and the gateway running.
2. (AC-2) `nibble sandbox attach --hermes` opens an interactive Hermes CLI session inside
   the running container.
3. (AC-3) The Hermes CLI inside the container can reach `localhost:6969` on the host
   (verified by `curl localhost:6969/health` or similar).
4. (AC-4) `~/.hermes/config.yaml` from the host is readable inside the container at
   `/home/node/.hermes/config.yaml`.
5. (AC-5) Repos listed in `[hermes] repos` config are mounted and accessible under `/repos/`.
6. (AC-6) After `nibble sandbox kill` + `nibble sandbox spawn --hermes`, the gateway
   restarts and previous Hermes sessions/memories are preserved (via `~/.hermes/` mount).
7. (AC-7) `nibble sandbox resume` after a host reboot restarts the Hermes container and
   the gateway process resumes.
8. (AC-8) Spawning a second `--hermes` sandbox while one exists reuses the existing container.
9. (AC-9) `cargo build` succeeds with no new warnings from the Hermes changes.
10. (AC-10) `cargo test` passes (existing + any new unit tests).

## Constraints

- **Performance**: Image build is one-time cost (~2-5 min for Python + Hermes install).
  Subsequent spawns reuse the cached image.
- **Security**: Hermes runs as non-root (`node` user, uid 1000) inside the container.
  `~/.hermes/.env` (API keys) is mounted read-write -- same trust model as current
  `~/.claude/` mounting.
- **Compatibility**: Requires Podman (same as existing nibble requirement). Hermes
  requires Python 3.11+.
- **Platform**: Linux only (same as nibble).

## Open Questions

1. Should the Hermes gateway logs be accessible via `nibble sandbox logs`? (Current
   implementation: yes, since gateway is PID 1, its stdout goes to container logs.)
2. Should we add `nibble inject` support for Hermes (send a message to the running
   agent)? Deferred to future work for now.
