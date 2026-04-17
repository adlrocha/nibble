# Blueprint: Hermes CLI Redesign — Dedicated Command Group

## Summary

Redesign the Hermes Agent integration as a dedicated `nibble hermes` command group,
decoupling it from the per-repo `nibble sandbox spawn` path. Hermes becomes a
**singleton, long-running sandbox** with its own gateway, where repos are dynamically
mounted/unmounted at runtime. Adding or removing repos requires a container restart
(transparent to the user, with a warning about in-progress tasks). Only the specific
repos the user explicitly mounts are accessible inside the container — maximum filesystem
isolation.

## Scope

### In scope
- New top-level CLI group: `nibble hermes {init,attach,mount,unmount,list,kill}`
- DB table `hermes_repos` to persist the mounted repo list across restarts
- `nibble hermes init` — spawn the singleton hermes sandbox (gateway as PID 1)
- `nibble hermes attach` — attach to the hermes CLI (auto-spawns if no sandbox exists)
- `nibble hermes mount <path>` — add a repo, restart container with new mount set
- `nibble hermes unmount <path>` — remove a repo, restart container with new mount set
- `nibble hermes list` — show sandbox status and mounted repos
- `nibble hermes kill` — stop and remove the hermes sandbox
- Remove `--hermes` flag from `nibble sandbox spawn` and `nibble sandbox attach`
- Migrate `HermesConfig.repos` from config.toml to DB-backed storage
- `HermesConfig` in config.toml retains only `gateway` and `image` settings
- Reuse `PodmanSandbox`, `SandboxConfig`, `container_state` table, and `AgentType::Hermes`
- Auto-spawn on `nibble hermes attach` (same pattern as `nibble sandbox attach`)

### Out of scope
- Hot-adding mounts without restart (Podman limitation — future: may explore overlay approaches)
- Multi-instance Hermes sandboxes (singleton remains enforced)
- Inject support for Hermes (deferred, as before)
- Telegram integration changes for hermes (unchanged — hermes sandboxes are detected by `AgentType`)
- Cron integration for hermes repos (future work)

### Dependencies
- Existing `PodmanSandbox` infrastructure (unchanged)
- `nibble-hermes:latest` Dockerfile (unchanged)
- `~/.hermes/` on host for persistent Hermes state

## Interfaces

### Public API / Exports — CLI

| Command | Input | Output | Errors |
|---------|-------|--------|--------|
| `nibble hermes init` | — | Spawns sandbox, prints task ID + container name | Podman unavailable, image build failure |
| `nibble hermes attach` | `--fresh` (optional) | Interactive hermes CLI session | No sandbox + spawn failure |
| `nibble hermes mount <path>` | Local path (required) | Restarting sandbox with repo added | Path doesn't exist, no sandbox running |
| `nibble hermes unmount <path>` | Local path (required) | Restarting sandbox with repo removed | Path not mounted, no sandbox running |
| `nibble hermes list` | — | Sandbox status + mounted repos table | — |
| `nibble hermes kill` | — | Stops and removes sandbox | No sandbox running |

### Data Types

```rust
// New DB table
// hermes_repos: persists which repos are mounted in the hermes sandbox
// Columns: id (PK), repo_path (canonical, unique), mount_name (basename, unique), created_at

// HermesConfig simplified (config.toml)
struct HermesConfig {
    gateway: bool,          // default: true
    image: String,          // default: "nibble-hermes:latest"
    // repos field REMOVED — now in DB
}
```

### Events / Side Effects

- **init**: Builds image if missing → creates container with gateway PID 1 → inserts task + container_state → auto-mounts repos from DB (or initial seed from config.toml on first run)
- **attach**: If no sandbox exists → auto-init first → exec into container with hermes CLI
- **mount**: Validates path exists → inserts into `hermes_repos` → kills container → re-spawns with updated mounts → warns user about restart
- **unmount**: Removes from `hermes_repos` → kills container → re-spawns with updated mounts → warns user about restart
- **kill**: Stops container → cleans up task + container_state (hermes_repos preserved for next init)

## State Machine / Flow

```
                    ┌──────────────────┐
                    │   No Sandbox     │
                    └────────┬─────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
        hermes init    hermes attach    hermes mount
              │         (auto-init)    (auto-init)
              │              │              │
              ▼              ▼              ▼
                    ┌──────────────────┐
                    │ Sandbox Running  │◄──────────────────┐
                    │ (gateway PID 1)  │                    │
                    └───┬──────┬───────┘                    │
                        │      │                            │
                 hermes attach  hermes mount/unmount        │
                        │      │                            │
                        │      ┼── kill container ──┐       │
                        │      │                    │       │
                        │      │    re-spawn with   │       │
                        │      │    new mount set   │       │
                        │      │                    │       │
                        │      └────────────────────┘       │
                        │                                   │
                  hermes kill                          hermes list
                        │                            (read-only, no state change)
                        ▼
                    ┌──────────────────┐
                    │   No Sandbox     │
                    │ (repos in DB     │
                    │  for next init)  │
                    └──────────────────┘
```

### Restart flow (mount/unmount)

```
1. User runs: nibble hermes mount /path/to/new-repo
2. Validate /path/to/new-repo exists on host
3. Insert into hermes_repos table
4. Print warning: "Restarting hermes sandbox to add /path/to/new-repo..."
5. Kill existing container (cmd_hermes_kill internal, no DB repo cleanup)
6. Re-spawn container with all repos from hermes_repos (cmd_hermes_spawn internal)
7. Print: "Hermes sandbox restarted with X repo(s) mounted"
```

## Invariants

1. (INV-1) **Singleton**: At most one hermes sandbox exists at any time. `init` and `attach`
   check for existing running hermes sandboxes and reuse them.
2. (INV-2) **Gateway PID 1**: When `gateway: true` (default), `hermes gateway` runs as the
   container's main process. Container stops if gateway crashes.
3. (INV-3) **Persistent repos**: The `hermes_repos` table survives `kill`. Repos are remounted
   on the next `init` or `attach`.
4. (INV-4) **Isolation**: Only repos in `hermes_repos` are mounted. No home directory, no
   broad parent directory. Each repo is bind-mounted as `/repos/<mount_name>:rw`.
5. (INV-5) **~/.hermes/ persistence**: The host's `~/.hermes/` is always mounted so Hermes
   sessions, memories, and config survive container restarts.
6. (INV-6) **Separate image**: Hermes uses `nibble-hermes:latest`, never `nibble-sandbox:latest`.
7. (INV-7) **No /workspace mount**: Hermes has no primary repo at `/workspace`. The working
   directory is `/home/node`. Repos live under `/repos/`.
8. (INV-8) **Restart warning**: `mount` and `unmount` always warn the user before restarting
   the container.
9. (INV-9) **Config migration**: If `hermes.repos` exists in config.toml on first `init` after
   upgrade, those repos are seeded into the DB table and the config field is ignored thereafter.

## Error Handling Strategy

| Error | Type | Handling |
|-------|------|----------|
| Podman not available | Expected | Bail with install instructions |
| Image build fails | Expected | Propagate error with context |
| `~/.hermes/` missing on host | Expected | Create minimal structure, warn to run `hermes setup` |
| Repo path doesn't exist for mount | Expected | Bail: "path does not exist" |
| Repo already mounted | Expected | Bail: "repo is already mounted at /repos/<name>" |
| Repo not found for unmount | Expected | Bail: "repo is not mounted" |
| No sandbox running for mount/unmount | Expected | Auto-init first, then proceed with mount/unmount |
| Gateway crashes | Expected | Container stops. `nibble hermes init` or `attach` re-spawns |
| Duplicate basenames | Expected | Append `-1`, `-2` suffixes (reuse existing logic) |
| Hermes sandbox already exists on init | Expected | Attach to existing (same as current INV-5 behavior) |
| User has running task during restart | Expected | Print warning prominently, proceed (user opted in via mount/unmount) |

## Acceptance Criteria

1. (AC-1) `nibble hermes init` creates a hermes sandbox with gateway running as PID 1.
2. (AC-2) `nibble hermes attach` opens an interactive hermes CLI. If no sandbox exists,
   it auto-spawns one first.
3. (AC-3) `nibble hermes attach --fresh` starts a new hermes session (no `--continue`).
4. (AC-4) `nibble hermes mount /path/to/repo` adds the repo to the DB, restarts the
   container, and the repo is accessible at `/repos/<name>` inside the container.
5. (AC-5) `nibble hermes unmount /path/to/repo` removes the repo, restarts the container,
   and `/repos/<name>` is no longer accessible.
6. (AC-6) `nibble hermes list` shows the sandbox status and all mounted repos.
7. (AC-7) `nibble hermes kill` stops the sandbox. `hermes_repos` in the DB are preserved.
8. (AC-8) After `kill` then `init` (or `attach`), all previously mounted repos are
   automatically re-mounted.
9. (AC-9) `nibble sandbox spawn --hermes` and `nibble sandbox attach --hermes` are removed
   or produce a deprecation message pointing to `nibble hermes`.
10. (AC-10) Repos from `hermes.repos` in config.toml are auto-seeded into `hermes_repos`
    on first `nibble hermes init` after upgrade.
11. (AC-11) `mount` and `unmount` print a clear warning before restarting the container.
12. (AC-12) `nibble hermes mount` on a non-existent path returns an error without modifying state.
13. (AC-13) `nibble hermes unmount` on a path that isn't mounted returns an error.
14. (AC-14) `cargo build` succeeds with no new warnings.
15. (AC-15) `cargo test` passes (existing + new unit tests for DB ops and config changes).

## Constraints

- **Security**: Only explicitly-mounted repos are accessible. No home directory mount.
  No broad parent directory mount. This is a hard constraint from the user.
- **Performance**: Container restart on mount/unmount takes ~2-5 seconds (Podman start).
  This is acceptable given that repo changes are infrequent.
- **Compatibility**: Same Podman + Linux requirements as existing nibble.
- **DB schema**: New `hermes_repos` table (schema v9 migration). Forward-compatible design.

## Open Questions

1. Should `nibble hermes mount` accept a `--name` flag to customize the mount point name?
   Default is the directory basename. Useful when basenames collide.
   **Recommendation**: Yes, add `--name <mount_name>` optional flag.
2. Should `nibble hermes mount` support non-directory paths (e.g., single files)?
   **Recommendation**: No — only directories for now. Hermes works with repos.
