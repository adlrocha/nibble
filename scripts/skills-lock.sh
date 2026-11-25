#!/usr/bin/env bash
# Regenerate skills-lock.json — a versioning + provenance manifest for every
# skill in skills/. Its fields follow the format used by the `skills` CLI
# (vercel-labs/skills), with git source metadata from skills-sources.json.
#
# Each skill records: source, sourceType, skillPath, version, license, and a
# sha256 of its SKILL.md (computedHash) so drift is detectable.
#
# Usage: scripts/skills-lock.sh                    # write skills-lock.json
#        scripts/skills-lock.sh --check            # exit non-zero if the lock differs from skills/ and the registry
#        scripts/skills-lock.sh --validate-sources # print validated source registry
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
[ -f "$SOURCES" ] || { echo "skills-sources.json is missing" >&2; exit 1; }
# Parse once and fail before writing anything if provenance is malformed.
if ! REGISTRY_JSON="$(jq -ec '
  def relative_path:
    type == "string" and length > 0 and (startswith("/") | not) and
    (split("/") | all(.[]; . != ".."));
  select(type == "object" and .version == 1 and (.skills | type == "object") and
    all(.skills | to_entries[];
      (.key | test("\\A[a-z0-9]+(-[a-z0-9]+)*\\z")) and
      (.value |
        type == "object" and
        (.repository | type == "string" and length > 0) and
        (.branch | type == "string" and length > 0) and
        (.path | relative_path) and
        (.revision | type == "string" and test("\\A([0-9a-f]{40})?\\z")) and
        (.license | type == "string" and length > 0) and
        ((has("license_file") | not) or (.license_file | relative_path))
      )
    ))' "$SOURCES")"; then
  echo "skills-sources.json is invalid" >&2
  exit 1
fi
if [ "${1:-}" = "--validate-sources" ]; then
  printf '%s\n' "$REGISTRY_JSON"
  exit 0
fi
SOURCES_JSON="$(jq -c '.skills' <<<"$REGISTRY_JSON")"
# Registered skills must exist; otherwise regenerating would erase their pins.
source_names="$(jq -r 'keys[]' <<<"$SOURCES_JSON")"
while IFS= read -r name; do
  [ -n "$name" ] || continue
  skill_dir="$SKILLS_DIR/$name"
  if [ -L "$skill_dir" ] || [ -L "$skill_dir/SKILL.md" ] || [ ! -f "$skill_dir/SKILL.md" ]; then
    echo "registered skill $name is missing a regular SKILL.md" >&2
    exit 1
  fi
done <<<"$source_names"

build_skill_object() {
  local name="$1" hash="$2"
  local source sourceType version license
  if jq -e --arg name "$name" 'has($name)' <<<"$SOURCES_JSON" >/dev/null; then
    source="$(jq -r --arg name "$name" '.[$name].repository' <<<"$SOURCES_JSON")"
    sourceType="git"
    version="$(jq -r --arg name "$name" '.[$name].revision' <<<"$SOURCES_JSON")"
    license="$(jq -r --arg name "$name" '.[$name].license' <<<"$SOURCES_JSON")"
  else
    local status=$?
    [ "$status" -eq 1 ] || { echo "could not read skill provenance for $name" >&2; return 1; }
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
