#!/usr/bin/env bash
# End-to-end tests for the one-command install path of install.sh: what it
# does when piped into bash (`curl … | bash`). The repository it fetches is a
# throwaway local one whose install.sh only records how it was called, so the
# tests never build or install anything.
#
#   tests/install_test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="$ROOT/install.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASSED=0
fail() {
    echo "FAIL: $*" >&2
    exit 1
}
pass() {
    PASSED=$((PASSED + 1))
    echo "ok - $*"
}

# A fake nibble repository: right Cargo.toml name, and an install.sh that
# writes its arguments, its directory, the current directory and whether
# stdin is a terminal to $RECORD.
UPSTREAM="$WORK/upstream"
git init --quiet --initial-branch=main "$UPSTREAM"
git -C "$UPSTREAM" config user.email test@example.com
git -C "$UPSTREAM" config user.name test
printf '[package]\nname = "nibble"\n' >"$UPSTREAM/Cargo.toml"
write_stub() {
    cat >"$UPSTREAM/install.sh" <<EOF
#!/bin/bash
{
    echo "version=$1"
    echo "args=\$*"
    echo "dir=\$(cd "\$(dirname "\$0")" && pwd)"
    echo "cwd=\$(pwd)"
    if [ -t 0 ]; then echo "stdin=tty"; else echo "stdin=other"; fi
} >"\$RECORD"
EOF
    git -C "$UPSTREAM" add -A
    git -C "$UPSTREAM" commit --quiet -m "$1"
}
write_stub v1
git -C "$UPSTREAM" tag v1
write_stub v2

export RECORD="$WORK/record"
export NIBBLE_REPO_URL="file://$UPSTREAM"
export NIBBLE_SRC_DIR="$WORK/data/nibble/src"
unset NIBBLE_REF

# Runs a command with no controlling terminal, so /dev/tty can't be opened
# (CI runners have none anyway).
detached() {
    if command -v setsid >/dev/null 2>&1; then setsid -w "$@"; else "$@"; fi
}
# Runs the installer the way `curl … | bash -s -- ARGS` does, from $WORK/cwd.
piped() {
    mkdir -p "$WORK/cwd"
    (cd "$WORK/cwd" && detached bash -s -- "$@" <"$INSTALLER") >"$WORK/out" 2>&1
}
field() { sed -n "s/^$1=//p" "$RECORD"; }

# First run clones main and hands over with the arguments.
piped --rebuild --recover backup.zip || fail "first install: $(cat "$WORK/out")"
[ -d "$NIBBLE_SRC_DIR/.git" ] || fail "source was not cloned"
[ "$(field version)" = v2 ] || fail "expected main (v2), got $(field version)"
[ "$(field args)" = "--rebuild --recover backup.zip" ] || fail "args: $(field args)"
[ "$(field dir)" = "$NIBBLE_SRC_DIR" ] || fail "ran from $(field dir)"
[ "$(field cwd)" = "$WORK/cwd" ] || fail "cwd changed to $(field cwd), relative paths would break"
[ "$(field stdin)" = other ] || fail "stdin should not be a terminal without one"
pass "first run clones main and runs its installer with the arguments"

# A later run updates the same checkout.
write_stub v3
piped || fail "update: $(cat "$WORK/out")"
[ "$(field version)" = v3 ] || fail "update installed $(field version)"
[ "$(field args)" = "" ] || fail "args: $(field args)"
pass "re-running updates the checkout to the latest main"

# --ref picks a tag, and is not passed on.
piped --ref v1 --browser || fail "--ref: $(cat "$WORK/out")"
[ "$(field version)" = v1 ] || fail "--ref v1 installed $(field version)"
[ "$(field args)" = "--browser" ] || fail "--ref leaked into args: $(field args)"
NIBBLE_REF=v1 piped || fail "NIBBLE_REF: $(cat "$WORK/out")"
[ "$(field version)" = v1 ] || fail "NIBBLE_REF=v1 installed $(field version)"
pass "--ref and NIBBLE_REF pick the branch or tag"

# A local edit in the managed checkout doesn't block an update.
echo "local edit" >>"$NIBBLE_SRC_DIR/Cargo.toml"
piped || fail "dirty update: $(cat "$WORK/out")"
[ "$(field version)" = v3 ] || fail "dirty checkout installed $(field version)"
pass "local changes in the managed checkout are overwritten"

# Running from inside a nibble checkout still fetches when piped.
rm -f "$RECORD"
(cd "$ROOT" && detached bash -s <"$INSTALLER") >"$WORK/out" 2>&1 || fail "piped in checkout: $(cat "$WORK/out")"
[ "$(field dir)" = "$NIBBLE_SRC_DIR" ] || fail "piped from a checkout ran $(field dir)"
pass "a pipe fetches even when the current directory is a nibble checkout"

# Failures are reported and nothing runs.
rm -f "$RECORD"
if piped --ref no-such-ref; then fail "a missing ref succeeded"; fi
grep -q "could not fetch no-such-ref" "$WORK/out" || fail "missing ref message: $(cat "$WORK/out")"
if piped --ref; then fail "--ref without a value succeeded"; fi
grep -q -- "--ref needs a branch or tag" "$WORK/out" || fail "--ref message: $(cat "$WORK/out")"
[ ! -e "$RECORD" ] || fail "an installer ran after a failed fetch"
pass "a bad ref fails before anything is installed"

rm -rf "$NIBBLE_SRC_DIR" && mkdir -p "$NIBBLE_SRC_DIR"
if piped; then fail "a non-git source dir succeeded"; fi
grep -q "is not a git checkout" "$WORK/out" || fail "non-git message: $(cat "$WORK/out")"
rm -rf "$NIBBLE_SRC_DIR"
NIBBLE_REPO_URL="file://$WORK/missing" piped && fail "a missing repository succeeded"
grep -q "could not clone" "$WORK/out" || fail "clone message: $(cat "$WORK/out")"
rm -rf "$NIBBLE_SRC_DIR"
git -C "$UPSTREAM" rm --quiet Cargo.toml && git -C "$UPSTREAM" commit --quiet -m "not nibble"
if piped; then fail "a non-nibble repository succeeded"; fi
grep -q "is not a nibble repository" "$WORK/out" || fail "non-nibble message: $(cat "$WORK/out")"
[ ! -e "$RECORD" ] || fail "an installer ran for a bad source"
pass "a bad source directory or repository is refused"

# From a checkout, --ref is refused instead of silently ignored.
if bash "$INSTALLER" --ref v1 </dev/null >"$WORK/out" 2>&1; then fail "--ref from a checkout succeeded"; fi
grep -q -- "--ref only applies to the one-command install" "$WORK/out" || fail "checkout --ref: $(cat "$WORK/out")"
pass "--ref from a checkout is refused"

echo "all $PASSED install tests passed"
