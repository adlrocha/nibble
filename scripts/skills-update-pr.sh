#!/usr/bin/env bash
# Check registered upstreams in a disposable worktree and open an update PR.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for cmd in git gh python3 pytest cargo jq; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "$cmd is required" >&2; exit 1; }
done
gh auth status >/dev/null

git -C "$ROOT" fetch --quiet origin main
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nibble-skills-pr.XXXXXXXX")"
cleanup() {
    git -C "$ROOT" worktree remove --force "$WORK/checkout" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT
git -C "$ROOT" worktree add --quiet --detach "$WORK/checkout" origin/main
cd "$WORK/checkout"
python3 scripts/skills-sync.py
bash scripts/skills-lock.sh --check
# This fresh worktree contains upstream skill assets, not the caller's ignored
# files. Include assets even when their names match nibble's ignore rules.
git add -f -- skills
git add -- skills-sources.json skills-lock.json
if git diff --cached --quiet; then
    echo "All registered skills are up to date; no PR needed."
    exit 0
fi

# A content-derived branch avoids duplicate PRs and never overwrites a branch.
branch="sync/skills-$(git diff --cached --binary | git hash-object --stdin | cut -c1-12)"
if [ -n "$(git ls-remote --heads origin "refs/heads/$branch")" ]; then
    echo "Branch $branch already exists; inspect its PR or remove it before retrying." >&2
    exit 1
fi

pytest -q tests/skills_sync_test.py
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}" cargo test
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}" cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check

git commit -m "chore(skills): sync vendored skills"
git push origin "HEAD:refs/heads/$branch"
gh pr create --base main --head "$branch" --title "chore(skills): sync vendored skills" \
    --body '## What changed?

Update vendored skills from their registered upstream revisions. Local skill edits are merged against the pinned bases; conflicts stop the sync.

## Verification

- Updated skill contents and provenance are in the diff.
- Ran the skill sync integration tests and repository checks.'
