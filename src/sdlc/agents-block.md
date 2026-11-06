## Delivery flow

This repository uses the `nibble-sdlc` skill: check the spec, build, open a PR, and loop with nibble-review until it LGTMs the current head. Each PR gets a `## Risk` comment from `land` saying whether it could merge without a person. Land is in shadow mode, so it only reports: never merge or enable auto-merge unless you are told to.
