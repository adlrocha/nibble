# Risk map

What merging to `main` sets off here, and what can't be undone. `nibble land` gives this to its judge. It is context, not rules: the rules are in `sdlc.toml`.

- `main` is what people install: the one-command installer fetches it. Nothing updates by itself, so a bad merge reaches a machine only when someone reinstalls.
- nibble runs coding agents with the owner's credentials. Sandbox mounts, environment forwarding, hooks, and the wrappers decide what those agents can reach on the host.
- `nibble rc` and `nibble cron` act on Slack messages and schedules with no one at the keyboard. Who may trigger them, and what runs, is their security boundary.
- `nibble land` merges pull requests with the owner's `gh`. Its rules, its judge's isolation, its record parsing, and its thresholds decide what merges in every repository that uses it.
- Skills under `skills/` are instructions every agent on the machine follows after the next install.
- Vendored skills run scripts from other repositories.
- Publishing a GitHub release is done by a person after the version bump merges.
