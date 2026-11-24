#!/usr/bin/env bash
# Regenerate skills-lock.json — a versioning + provenance manifest for every
# skill in skills/. Its fields follow the format used by the `skills` CLI
# (vercel-labs/skills), with git source metadata from skills-sources.json.
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

# External skill provenance lives in skills-sources.json. Other skills are
# maintained in this checkout. Keep this compatible with macOS's Bash 3.
SOURCES="$REPO_DIR/skills-sources.json"

build_skill_object() {
  local name="$1" hash="$2"
  local source sourceType version license
  if [ -f "$SOURCES" ] && jq -e --arg name "$name" '.skills[$name] != null' "$SOURCES" >/dev/null; then
    source="$(jq -r --arg name "$name" '.skills[$name].repository' "$SOURCES")"
    sourceType="git"
    version="$(jq -r --arg name "$name" '.skills[$name].revision' "$SOURCES")"
    license="$(jq -r --arg name "$name" '.skills[$name].license' "$SOURCES")"
  else
    source="vendored:local"
    sourceType="local"
    version="unpinned"
    case "$name" in
      engineering|nibble-memory|nibble-pr-review|nibble-sdlc|omarchy-migration) license="proprietary" ;;
      *) license="unknown" ;;
    esac
  fi
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
  if command -v sha256sum >/dev/null 2>&1; then
    hash="$(sha256sum "$skill_file" | awk '{print $1}')"
  else
    hash="$(shasum -a 256 "$skill_file" | awk '{print $1}')"
  fi
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
