# QA Gate: Hermes CLI Redesign

**Date**: 2026-04-17
**Reviewer**: Human
**Result**: APPROVED WITH CHANGES

## Items Reviewed

| # | Function | Risk | Decision | Notes |
|---|----------|------|----------|-------|
| 1 | cmd_hermes_spawn_internal | High | Changes requested | tempdir accumulation → persistent dir |
| 2 | mount/unmount restart flow | High | Changes requested | add confirmation prompt |

## Change Requests

### CR-1: workspace tempdir accumulation
- **Description**: Replace `tempfile::tempdir()` with a persistent `~/.nibble/hermes/workspace/` directory that is reused across restarts.
- **Status**: Implemented

### CR-2: confirmation prompt for mount/unmount
- **Description**: Add interactive `[y/N]` confirmation prompt before restarting the sandbox. Add `--yes` flag to skip for scripting.
- **Status**: Implemented

## Unresolved Items
None
