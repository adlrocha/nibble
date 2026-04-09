# QA Gate Report — factory-pipeline-integration

**Date:** 2026-04-08  
**Feature:** AI Factory Pipeline integration into nibble  
**Pipeline run:** retrospective (pipeline run on itself)  
**Gate decision:** APPROVED — all items resolved

---

## Items Reviewed

### Item 1 — `inject_sandbox_claude_md` (Critical, 22/25)

**Finding:** Two bugs in `inject_sandbox_claude_md` (`src/main.rs`):
1. **FIND-1:** `awk -v` assignment mangles backslashes — awk interprets `\n`, `\t`, `\\` in `-v` values, corrupting CLAUDE.md content.
2. **FIND-2:** `grep -qF "@AGENTS.md"` was not anchored to line 1 — if `@AGENTS.md` appeared anywhere in the body, the import was never prepended.

**Decision:** Request changes  
**Changes applied:**
- FIND-1: Replaced `awk -v content=...` with writing content to a tmpfile and reading via `getline`
- FIND-2: Replaced `grep -qF "@AGENTS.md"` with `head -1 "$TARGET" | grep -qF "@AGENTS.md"`

**Re-verification:** Tests pass (77/77). Logic confirmed correct.  
**Final decision:** ✅ Approved

---

### Item 2 — `cmd_sandbox_spawn` factory path (High, 17/25)

**Finding:** Non-fatal failure when factory pipeline is enabled — if `inject_sandbox_claude_md` fails, spawn continues without factory instructions in the container.

**Assessment:** Failure mode is acceptable; sandbox still spawns and user is informed. Silent degradation is a known, documented tradeoff.

**Decision:** ✅ Approved as-is

---

### Item 3 — `install.sh` global AGENTS.md (High)

**Finding (FIND-4):** `install.sh` was copying the full sandbox `AGENTS.md` (including sandbox-specific environment/toolchain/port/sudo blocks) to `~/.config/opencode/AGENTS.md`, leaking irrelevant instructions into host-side OpenCode sessions.

**Decision:** Request changes  
**Changes applied:**
- Added `<!-- nibble:global:begin -->` / `<!-- nibble:global:end -->` sentinels to `AGENTS.md` (lines 40, 102) around the factory pipeline section
- Updated `install.sh` step 4b to extract only the sentinel-bounded section via `awk`
- `build_sandbox_agents_md` in `src/main.rs` now also emits sentinels in generated container AGENTS.md for consistency
- 9 new unit tests added for `build_sandbox_agents_md` and `build_sandbox_claude_md`
- Deleted `AGENTS.global.md` (sentinel approach is the single source of truth)

**Re-verification:** Tests pass (77/77). Sentinel extraction confirmed at `install.sh:178-179`.  
**Final decision:** ✅ Approved

---

## Gate Criteria Assessment

| Criterion | Status |
|-----------|--------|
| All Critical risks resolved or accepted with documented rationale | ✅ |
| All High risks resolved or accepted with documented rationale | ✅ |
| Test suite passes | ✅ 77/77 |
| No regressions introduced by fixes | ✅ |
| All request-changes items re-verified before re-approval | ✅ |

## Overall Decision: **SHIP** ✅

All pipeline stages complete. Feature is approved to merge/deploy.
