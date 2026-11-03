#!/usr/bin/env bash
# End-to-end tests for the one-command install path of install.sh (what it
# does when piped into bash, as in `curl … | bash`) and for uninstall.sh's
# cleanup of the checkout it creates.
#
# The repository the installer fetches is a throwaway local one. Its
# install.sh is the real script's preamble (everything before flag parsing:
# fetching, checkout detection, the re-run hints) followed by lines that
# record how it was run, so nothing is ever built or installed.
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
skip() { echo "skip - $*"; }

PREAMBLE="$(sed '/^# ── Parse flags/,$d' "$INSTALLER")"
grep -q 'fetch_and_install "\$@"' <<<"$PREAMBLE" || fail "install.sh preamble not found"

UPSTREAM="$WORK/upstream"
git init --quiet --initial-branch=main "$UPSTREAM"
git -C "$UPSTREAM" config user.email test@example.com
git -C "$UPSTREAM" config user.name test
printf '[package]\nname = "nibble"\n' >"$UPSTREAM/Cargo.toml"
# Commits an install.sh that reports as version $1.
write_stub() {
    {
        printf '%s\n' "$PREAMBLE"
        cat <<EOF
{
    echo "version=$1"
    echo "args=\$*"
    echo "dir=\$REPO_DIR"
    echo "cwd=\$(pwd)"
    echo "update=\$RERUN"
    echo "rebuild=\$(rerun_with --rebuild)"
    if [ -t 0 ]; then echo "stdin=tty"; else echo "stdin=other"; fi
} >"\$RECORD"
EOF
    } >"$UPSTREAM/install.sh"
    git -C "$UPSTREAM" add -A
    git -C "$UPSTREAM" commit --quiet -m "$1"
}
write_stub v1
git -C "$UPSTREAM" tag v1
write_stub v2

export RECORD="$WORK/record"
export NIBBLE_REPO_URL="file://$UPSTREAM"
export NIBBLE_SRC_DIR="$WORK/data/nibble/src"
unset NIBBLE_REF XDG_DATA_HOME

# Runs a command without a controlling terminal where the platform allows
# it (setsid is Linux-only; CI runners have no terminal anyway).
detached() {
    if command -v setsid >/dev/null 2>&1; then setsid -w "$@"; else "$@"; fi
}
# What the installer's stdin should be under `detached`.
if detached bash -c '(exec </dev/tty) 2>/dev/null'; then NO_TTY_STDIN="tty"; else NO_TTY_STDIN="other"; fi
# Runs the installer the way `curl … | bash -s -- ARGS` does, from $WORK/cwd.
piped() {
    mkdir -p "$WORK/cwd"
    (cd "$WORK/cwd" && detached bash -s -- "$@" <"$INSTALLER") >"$WORK/out" 2>&1
}
field() { sed -n "s/^$1=//p" "$RECORD"; }
ONE="curl -fsSL https://raw.githubusercontent.com/adlrocha/nibble/main/install.sh |"
SRC_ENV="NIBBLE_REPO_URL=file://$UPSTREAM NIBBLE_SRC_DIR=$NIBBLE_SRC_DIR"

# First run clones main and hands over with the arguments.
piped --rebuild --recover backup.zip || fail "first install: $(cat "$WORK/out")"
[ -f "$NIBBLE_SRC_DIR/.git/nibble-installer-managed" ] || fail "source was not cloned and marked"
[ "$(field version)" = v2 ] || fail "expected main (v2), got $(field version)"
[ "$(field args)" = "--rebuild --recover backup.zip" ] || fail "args: $(field args)"
[ "$(field dir)" = "$NIBBLE_SRC_DIR" ] || fail "ran from $(field dir)"
[ "$(field cwd)" = "$WORK/cwd" ] || fail "cwd changed to $(field cwd), relative paths would break"
[ "$(field stdin)" = "$NO_TTY_STDIN" ] || fail "stdin: $(field stdin), expected $NO_TTY_STDIN"
pass "first run clones main and runs its installer with the arguments"

# The hints repeat the one-command install with the same source.
[ "$(field update)" = "$ONE $SRC_ENV bash" ] || fail "update hint: $(field update)"
[ "$(field rebuild)" = "$ONE $SRC_ENV bash -s -- --rebuild" ] || fail "rebuild hint: $(field rebuild)"
pass "hints name the one-command install with the same source"

# A later run updates the same checkout.
write_stub v3
piped || fail "update: $(cat "$WORK/out")"
[ "$(field version)" = v3 ] || fail "update installed $(field version)"
[ "$(field args)" = "" ] || fail "args: $(field args)"
pass "re-running updates the checkout to the latest main"

# --ref picks a tag, is not passed on, and stays in the hints.
piped --ref v1 --browser || fail "--ref: $(cat "$WORK/out")"
[ "$(field version)" = v1 ] || fail "--ref v1 installed $(field version)"
[ "$(field args)" = "--browser" ] || fail "--ref leaked into args: $(field args)"
[ "$(field update)" = "$ONE $SRC_ENV bash -s -- --ref v1" ] || fail "pinned update hint: $(field update)"
[ "$(field rebuild)" = "$ONE $SRC_ENV bash -s -- --ref v1 --rebuild" ] || fail "pinned rebuild hint: $(field rebuild)"
NIBBLE_REF=v1 piped || fail "NIBBLE_REF: $(cat "$WORK/out")"
[ "$(field version)" = v1 ] || fail "NIBBLE_REF=v1 installed $(field version)"
pass "--ref and NIBBLE_REF pick the branch or tag, and the hints keep it"

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

# With a terminal, the installer gets it, so its prompts work.
under_pty() { script -qec "$1" /dev/null; }
script --version >/dev/null 2>&1 || under_pty() { script -q /dev/null bash -c "$1"; }
if under_pty '[ -t 0 ]' >/dev/null 2>&1 </dev/null; then
    rm -f "$RECORD"
    under_pty "bash -s <'$INSTALLER'" >"$WORK/out" 2>&1 </dev/null || fail "pty install: $(cat "$WORK/out")"
    [ "$(field stdin)" = tty ] || fail "the installer did not get the terminal: $(field stdin)"
    pass "the installer reads from the terminal when there is one"
else
    skip "no pseudo-terminal available to test the terminal hand-over"
fi

# Failures are reported and nothing runs.
rm -f "$RECORD"
if piped --ref no-such-ref; then fail "a missing ref succeeded"; fi
grep -q "could not fetch no-such-ref" "$WORK/out" || fail "missing ref message: $(cat "$WORK/out")"
if piped --ref; then fail "--ref without a value succeeded"; fi
grep -q -- "--ref needs a branch or tag" "$WORK/out" || fail "--ref message: $(cat "$WORK/out")"
[ ! -e "$RECORD" ] || fail "an installer ran after a failed fetch"
pass "a bad ref fails before anything is installed"

# A directory the installer didn't create is never reset, git or not.
MANAGED="$NIBBLE_SRC_DIR"
export NIBBLE_SRC_DIR="$WORK/mine"
git clone --quiet "$UPSTREAM" "$NIBBLE_SRC_DIR"
git -C "$NIBBLE_SRC_DIR" remote set-url origin https://example.com/my-fork.git
echo "work in progress" >>"$NIBBLE_SRC_DIR/Cargo.toml"
if piped; then fail "an unmanaged clone was updated"; fi
grep -q "was not created by this installer" "$WORK/out" || fail "unmanaged message: $(cat "$WORK/out")"
grep -q "work in progress" "$NIBBLE_SRC_DIR/Cargo.toml" || fail "uncommitted work was lost"
[ "$(git -C "$NIBBLE_SRC_DIR" remote get-url origin)" = https://example.com/my-fork.git ] || fail "origin was rewritten"
rm -rf "$NIBBLE_SRC_DIR" && mkdir -p "$NIBBLE_SRC_DIR"
if piped; then fail "a plain directory was used"; fi
grep -q "was not created by this installer" "$WORK/out" || fail "plain dir message: $(cat "$WORK/out")"
pass "a checkout or directory the installer didn't create is left alone"

rm -rf "$NIBBLE_SRC_DIR"
NIBBLE_REPO_URL="file://$WORK/missing" piped && fail "a missing repository succeeded"
grep -q "could not clone" "$WORK/out" || fail "clone message: $(cat "$WORK/out")"
rm -rf "$NIBBLE_SRC_DIR"
git clone --quiet "$UPSTREAM" "$WORK/not-nibble"
git -C "$WORK/not-nibble" rm --quiet Cargo.toml
git -C "$WORK/not-nibble" -c user.email=t@e -c user.name=t commit --quiet -m "not nibble"
if NIBBLE_REPO_URL="file://$WORK/not-nibble" piped; then fail "a non-nibble repository succeeded"; fi
grep -q "is not a nibble repository" "$WORK/out" || fail "non-nibble message: $(cat "$WORK/out")"
[ ! -e "$RECORD" ] || fail "an installer ran for a bad source"
pass "a missing or non-nibble repository is refused"
export NIBBLE_SRC_DIR="$MANAGED"

# From a checkout, --ref and NIBBLE_REF are refused instead of ignored.
if bash "$INSTALLER" --ref v1 </dev/null >"$WORK/out" 2>&1; then fail "--ref from a checkout succeeded"; fi
grep -q -- "--ref only applies to the one-command install" "$WORK/out" || fail "checkout --ref: $(cat "$WORK/out")"
if NIBBLE_REF=v1 bash "$INSTALLER" </dev/null >"$WORK/out" 2>&1; then fail "NIBBLE_REF from a checkout succeeded"; fi
grep -q "NIBBLE_REF only applies to the one-command install" "$WORK/out" || fail "checkout NIBBLE_REF: $(cat "$WORK/out")"
pass "--ref and NIBBLE_REF from a checkout are refused"

# uninstall.sh deletes the managed checkout and nothing else. It runs with a
# throwaway HOME and stubs for the commands that reach system state.
STUBS="$WORK/stubs"
mkdir -p "$STUBS" "$WORK/home"
for cmd in systemctl podman nibble; do printf '#!/bin/sh\nexit 0\n' >"$STUBS/$cmd" && chmod +x "$STUBS/$cmd"; done
uninstall() {
    env -u XDG_DATA_HOME -u XDG_CONFIG_HOME HOME="$WORK/home" PATH="$STUBS:$PATH" \
        bash "$ROOT/uninstall.sh" </dev/null >"$WORK/out" 2>&1
}
[ -f "$NIBBLE_SRC_DIR/.git/nibble-installer-managed" ] || fail "no managed checkout to uninstall"
uninstall || fail "uninstall: $(cat "$WORK/out")"
[ ! -e "$NIBBLE_SRC_DIR" ] || fail "uninstall left the managed checkout"
[ ! -e "$(dirname "$NIBBLE_SRC_DIR")" ] || fail "uninstall left the empty parent directory"
grep -q "raw.githubusercontent.com/adlrocha/nibble/main/install.sh | bash" "$WORK/out" || fail "reinstall hint: $(cat "$WORK/out")"
export NIBBLE_SRC_DIR="$WORK/mine"
rm -rf "$NIBBLE_SRC_DIR"
git clone --quiet "$UPSTREAM" "$NIBBLE_SRC_DIR"
uninstall || fail "uninstall with a clone: $(cat "$WORK/out")"
[ -f "$NIBBLE_SRC_DIR/Cargo.toml" ] || fail "uninstall deleted a clone it didn't create"
grep -q "not created by install.sh" "$WORK/out" || fail "keep message: $(cat "$WORK/out")"
pass "uninstall.sh removes the managed checkout and keeps any other"

echo "all $PASSED install tests passed"
