#!/usr/bin/env bash
# Regenerate skills-lock.json — a versioning + provenance manifest for every
# skill in skills/. Mirrors the format used by the `skills` CLI
# (vercel-labs/skills) and shellment-platform's skills-lock.json, so nibble can
# adopt that CLI later without changing the manifest.
#
# Each skill records: source, sourceType, skillPath, version, license, and a
# sha256 of its SKILL.md (computedHash) so drift is detectable.
#
# Usage: scripts/skills-lock.sh          # write skills-lock.json
#        scripts/skills-lock.sh --check  # exit non-zero if locked hashes differ
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKILLS_DIR="$REPO_DIR/skills"
LOCK="$REPO_DIR/skills-lock.json"
CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }

# Provenance table: skill_name -> "source|sourceType|version|license".
# Add an entry here when you vendor or pin a new skill. Unknown skills default
# to a local/vendored record.
declare -A PROVENANCE=(
  ["engineering"]="vendored:local|local|unpinned|proprietary"
  ["git-commit-pr"]="we-are-singular/skills|github|vendored-2026-07|MIT"
  ["nibble-memory"]="vendored:local|local|unpinned|proprietary"
  ["nibble-pr-review"]="vendored:local|local|unpinned|proprietary"
  ["omarchy-migration"]="vendored:local|local|unpinned|proprietary"
)

build_skill_object() {
  local name="$1" hash="$2"
  local meta="${PROVENANCE[$name]:-vendored:local|local|unpinned|unknown}"
  local source sourceType version license
  IFS='|' read -r source sourceType version license <<<"$meta"
  jq -n \
    --arg source "$source" \
    --arg sourceType "$sourceType" \
    --arg skillPath "skills/$name/SKILL.md" \
    --arg version "$version" \
    --arg license "$license" \
    --arg hash "$hash" \
    '{source:$source, sourceType:$sourceType, skillPath:$skillPath, version:$version, license:$license, computedHash:$hash}'
}

# Build the skills object by folding each skill into an accumulator.
skills_json='{}'
for skill_dir in "$SKILLS_DIR"/*/; do
  [ -d "$skill_dir" ] || continue
  skill_file="$skill_dir/SKILL.md"
  [ -f "$skill_file" ] || continue
  name="$(basename "$skill_dir")"
  hash="$(sha256sum "$skill_file" | awk '{print $1}')"
  obj="$(build_skill_object "$name" "$hash")"
  skills_json="$(jq --arg name "$name" --argjson obj "$obj" '. + {($name): $obj}' <<<"$skills_json")"
done

new_lock="$(jq -n --argjson skills "$skills_json" '{version:1, skills:$skills}')"

if [ "$CHECK" -eq 1 ]; then
  if [ ! -f "$LOCK" ]; then
    echo "skills-lock.json missing; run scripts/skills-lock.sh" >&2
    exit 1
  fi
  if ! diff <(jq -S . "$LOCK") <(jq -S . <<<"$new_lock") >/dev/null; then
    echo "skills-lock.json out of date; run scripts/skills-lock.sh" >&2
    diff <(jq -r '.skills|to_entries[]|"\(.key) \(.value.computedHash)"' "$LOCK" | sort) \
         <(jq -r '.skills|to_entries[]|"\(.key) \(.value.computedHash)"' <<<"$new_lock" | sort) >&2 || true
    exit 1
  fi
  echo "skills-lock.json up to date"
  exit 0
fi

jq . <<<"$new_lock" >"$LOCK"
echo "Wrote $LOCK ($(jq '.skills|length' "$LOCK") skills)"
