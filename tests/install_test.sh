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
# The cut must end after the hints and before any install step, or the stub
# below would run real install steps.
{ grep -q '^RERUN=' <<<"$PREAMBLE" && ! grep -q '^step "Checking prerequisites"' <<<"$PREAMBLE"; } \
    || fail "install.sh preamble split changed: expected RERUN= before '# ── Parse flags'"

UPSTREAM="$WORK/upstream"
git init --quiet --initial-branch=main "$UPSTREAM"
git -C "$UPSTREAM" config user.email test@example.com
git -C "$UPSTREAM" config user.name test
printf '[package]\nname = "nibble"\n' >"$UPSTREAM/Cargo.toml"
cp "$ROOT/uninstall.sh" "$UPSTREAM/uninstall.sh"
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
    echo "caller=\$CALLER_DIR"
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
[ "$(field cwd)" = "$NIBBLE_SRC_DIR" ] || fail "the install ran from $(field cwd), not its checkout"
[ "$(field caller)" = "$WORK/cwd" ] || fail "caller dir: $(field caller)"
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
if piped --ref --rebuild; then fail "--ref took a flag as its value"; fi
grep -q -- "--ref needs a branch or tag" "$WORK/out" || fail "--ref flag message: $(cat "$WORK/out")"
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
if NIBBLE_REF=v1 bash "$NIBBLE_SRC_DIR/install.sh" </dev/null >"$WORK/out" 2>&1; then
    fail "a hand-set NIBBLE_REF in the managed checkout succeeded"
fi
grep -q "NIBBLE_REF only applies to the one-command install" "$WORK/out" || fail "managed NIBBLE_REF: $(cat "$WORK/out")"
pass "--ref and NIBBLE_REF from a checkout are refused, the managed one too"

# Run through a symlink, it installs the checkout the link points into
# (shown here by refusing NIBBLE_REF instead of fetching); a copy outside
# any crate is refused.
mkdir -p "$WORK/bin" "$WORK/lonely"
ln -s "$INSTALLER" "$WORK/bin/install.sh"
rm -f "$RECORD"
if (cd "$WORK" && NIBBLE_REF=v1 bash bin/install.sh </dev/null >"$WORK/out" 2>&1); then fail "symlinked run fetched"; fi
grep -q "NIBBLE_REF only applies" "$WORK/out" || fail "symlinked run: $(cat "$WORK/out")"
cp "$INSTALLER" "$WORK/lonely/install.sh"
if bash "$WORK/lonely/install.sh" </dev/null >"$WORK/out" 2>&1; then fail "a copy outside a checkout ran"; fi
grep -q "is not a nibble checkout" "$WORK/out" || fail "lonely copy: $(cat "$WORK/out")"
[ ! -e "$RECORD" ] || fail "a symlinked or stray installer fetched"
pass "a symlinked installer resolves its checkout; a stray copy is refused"

# From a clone, hints name its install.sh, relative only when run from it.
CLONE="$WORK/clone"
rm -rf "$CLONE" && git clone --quiet "$UPSTREAM" "$CLONE"
rm -f "$RECORD"
(cd "$WORK" && bash clone/install.sh </dev/null >"$WORK/out" 2>&1) || fail "clone run: $(cat "$WORK/out")"
[ "$(field update)" = "$CLONE/install.sh" ] || fail "clone hint from elsewhere: $(field update)"
[ "$(field cwd)" = "$CLONE" ] || fail "clone install ran from $(field cwd)"
(cd "$CLONE" && bash ./install.sh </dev/null >"$WORK/out" 2>&1) || fail "clone run: $(cat "$WORK/out")"
[ "$(field rebuild)" = "./install.sh --rebuild" ] || fail "clone hint from inside: $(field rebuild)"
pass "a clone's hints name its own install.sh"

# Exercise the installer's real skill-copy function in an isolated HOME.
awk '/^install_skill\(\) \{/ { copying=1 } copying { print } copying && /^}/ { exit }' "$INSTALLER" >"$WORK/install-skill.sh"
mkdir -p "$WORK/skill-source/example/references" "$WORK/skill-dest"
printf 'version 1\n' >"$WORK/skill-source/example/SKILL.md"
printf 'reference\n' >"$WORK/skill-source/example/references/guide.md"
(
    # Used indirectly by the extracted installer function.
    # shellcheck disable=SC2329
    ok() { :; }
    # This file is generated from install.sh above.
    # shellcheck disable=SC1091
    source "$WORK/install-skill.sh"
    install_skill "$WORK/skill-source/example/" "$WORK/skill-dest"
    [ -f "$WORK/skill-dest/example/references/guide.md" ] || fail "install omitted a reference"
    printf 'version 2\n' >"$WORK/skill-source/example/SKILL.md"
    rm "$WORK/skill-source/example/references/guide.md"
    install_skill "$WORK/skill-source/example/" "$WORK/skill-dest"
    grep -q 'version 2' "$WORK/skill-dest/example/SKILL.md" || fail "install did not upgrade skill"
    [ ! -e "$WORK/skill-dest/example/references/guide.md" ] || fail "install left an old reference"
)
pass "install refreshes complete skill directories"

# uninstall.sh deletes the managed checkout and nothing else. It runs with a
# throwaway HOME and stubs for the commands that reach system state.
STUBS="$WORK/stubs"
mkdir -p "$STUBS" "$WORK/home"
for cmd in systemctl podman nibble; do printf '#!/bin/sh\nexit 0\n' >"$STUBS/$cmd" && chmod +x "$STUBS/$cmd"; done
# Runs uninstall.sh ($1, default this repository's) like a user would.
uninstall() {
    env -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u XDG_STATE_HOME -u XDG_CACHE_HOME HOME="$WORK/home" PATH="$STUBS:$PATH" \
        bash "${1:-$ROOT/uninstall.sh}" </dev/null >"$WORK/out" 2>&1
}
[ -f "$NIBBLE_SRC_DIR/.git/nibble-installer-managed" ] || fail "no managed checkout to uninstall"
# What `nibble cron` leaves behind goes too.
mkdir -p "$WORK/home/.config/nibble" "$WORK/home/.local/state/nibble/cron"
touch "$WORK/home/.config/nibble/cron-secrets.toml" "$WORK/home/.config/nibble/cron.env" \
    "$WORK/home/.local/state/nibble/cron/cron.db"
# So does `nibble session search`'s transcript cache.
mkdir -p "$WORK/home/.cache/nibble"
touch "$WORK/home/.cache/nibble/session-search.json"
# Bundled skills are removed, while unrelated third-party skills remain.
for dir in "$WORK/home/.claude/skills" "$WORK/home/.nibble/skills" "$WORK/home/.pi/agent/skills"; do
    mkdir -p "$dir/inference-optimisation" "$dir/skill-distill" "$dir/third-party"
done
uninstall || fail "uninstall: $(cat "$WORK/out")"
for dir in "$WORK/home/.claude/skills" "$WORK/home/.nibble/skills" "$WORK/home/.pi/agent/skills"; do
    [ ! -e "$dir/inference-optimisation" ] || fail "uninstall left inference-optimisation in $dir"
    [ ! -e "$dir/skill-distill" ] || fail "uninstall left skill-distill in $dir"
    if [ "$dir" != "$WORK/home/.nibble/skills" ]; then
        [ -d "$dir/third-party" ] || fail "uninstall removed an unrelated skill from $dir"
    fi
done
[ ! -e "$WORK/home/.cache/nibble/session-search.json" ] || fail "uninstall left the session search cache"
[ ! -e "$WORK/home/.config/nibble" ] || fail "uninstall left cron's secrets: $(ls -A "$WORK/home/.config/nibble")"
[ ! -e "$WORK/home/.local/state/nibble" ] || fail "uninstall left cron's state"
[ ! -e "$NIBBLE_SRC_DIR" ] || fail "uninstall left the managed checkout"
[ ! -e "$(dirname "$NIBBLE_SRC_DIR")" ] || fail "uninstall left the empty parent directory"
grep -q "$ROOT/install.sh" "$WORK/out" || fail "a clone's reinstall hint: $(cat "$WORK/out")"
export NIBBLE_SRC_DIR="$WORK/mine"
rm -rf "$NIBBLE_SRC_DIR"
git clone --quiet "$UPSTREAM" "$NIBBLE_SRC_DIR"
uninstall || fail "uninstall with a clone: $(cat "$WORK/out")"
[ -f "$NIBBLE_SRC_DIR/Cargo.toml" ] || fail "uninstall deleted a clone it didn't create"
grep -q "not created by install.sh" "$WORK/out" || fail "keep message: $(cat "$WORK/out")"
pass "uninstall.sh removes the managed checkout and keeps any other"

# New bundled skill names are discovered even when uninstall is symlinked.
mkdir -p "$WORK/skill-checkout/skills/future-skill" "$WORK/home/.claude/skills/future-skill"
printf '%s\n' '---' >"$WORK/skill-checkout/skills/future-skill/SKILL.md"
cp "$ROOT/uninstall.sh" "$WORK/skill-checkout/uninstall.sh"
ln -s "$WORK/skill-checkout/uninstall.sh" "$WORK/bin/uninstall.sh"
uninstall "$WORK/bin/uninstall.sh" || fail "symlinked uninstall: $(cat "$WORK/out")"
[ ! -e "$WORK/home/.claude/skills/future-skill" ] || fail "symlinked uninstall left a discovered skill"
pass "uninstall discovers bundled skills through a symlink"

# Discovered names are literal, even if they contain whitespace or glob characters.
mkdir -p "$WORK/names-cwd/globZskill" "$WORK/home/.claude/skills/globZskill"
for name in 'odd skill' 'glob*skill'; do
    mkdir -p "$WORK/skill-checkout/skills/$name" "$WORK/home/.claude/skills/$name"
    printf '%s\n' '---' >"$WORK/skill-checkout/skills/$name/SKILL.md"
done
(cd "$WORK/names-cwd" && uninstall "$WORK/bin/uninstall.sh") || fail "uninstall with unusual skill names: $(cat "$WORK/out")"
[ ! -e "$WORK/home/.claude/skills/odd skill" ] || fail "uninstall left a space-containing bundled skill"
[ ! -e "$WORK/home/.claude/skills/glob*skill" ] || fail "uninstall left a glob-containing bundled skill"
[ -d "$WORK/home/.claude/skills/globZskill" ] || fail "uninstall expanded a skill name and removed a third-party skill"
pass "uninstall treats discovered skill names literally"

# A piped uninstaller must not treat the caller's project as its own checkout.
mkdir -p "$WORK/foreign/skills/third-party" "$WORK/home/.claude/skills/third-party" "$WORK/home/.pi/agent/skills/third-party"
printf '%s\n' '---' >"$WORK/foreign/skills/third-party/SKILL.md"
(cd "$WORK/foreign" && env -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u XDG_STATE_HOME -u XDG_CACHE_HOME \
    HOME="$WORK/home" PATH="$STUBS:$PATH" NIBBLE_SRC_DIR="$WORK/no-checkout" \
    bash -s -- --keep-config <"$ROOT/uninstall.sh" >"$WORK/out" 2>&1) \
    || fail "piped uninstall: $(cat "$WORK/out")"
[ -d "$WORK/home/.claude/skills/third-party" ] || fail "piped uninstall deleted a Claude skill from the caller's project"
[ -d "$WORK/home/.pi/agent/skills/third-party" ] || fail "piped uninstall deleted a Pi skill from the caller's project"
grep -q 'raw.githubusercontent.com/adlrocha/nibble/main/install.sh | bash' "$WORK/out" || fail "piped uninstall reinstall hint"
pass "piped uninstall leaves caller project skills alone"

# A checkout installed to a custom NIBBLE_SRC_DIR is removed by its own
# uninstall.sh without setting the variable again.
export NIBBLE_SRC_DIR="$WORK/custom/src"
piped || fail "custom install: $(cat "$WORK/out")"
unset NIBBLE_SRC_DIR
uninstall "$WORK/custom/src/uninstall.sh" || fail "custom uninstall: $(cat "$WORK/out")"
[ ! -e "$WORK/custom/src" ] || fail "uninstall left a checkout in a custom NIBBLE_SRC_DIR"
grep -q "raw.githubusercontent.com/adlrocha/nibble/main/install.sh | bash" "$WORK/out" || fail "piped reinstall hint: $(cat "$WORK/out")"
pass "uninstall.sh finds a custom checkout from where it runs"

# Run directly, a managed checkout in a custom place hints at itself.
export NIBBLE_SRC_DIR="$WORK/custom/src"
piped || fail "custom install: $(cat "$WORK/out")"
unset NIBBLE_SRC_DIR NIBBLE_REPO_URL
(cd "$WORK" && bash custom/src/install.sh </dev/null >"$WORK/out" 2>&1) || fail "direct run: $(cat "$WORK/out")"
[ "$(field update)" = "$ONE NIBBLE_REPO_URL=file://$UPSTREAM NIBBLE_SRC_DIR=$WORK/custom/src bash" ] \
    || fail "direct-run hint: $(field update)"
NIBBLE_REPO_URL="file://$UPSTREAM" NIBBLE_SRC_DIR="$WORK/custom/src" piped --ref v1 || fail "pinned: $(cat "$WORK/out")"
(cd "$WORK" && bash custom/src/install.sh </dev/null >"$WORK/out" 2>&1) || fail "pinned direct run: $(cat "$WORK/out")"
[ "$(field rebuild)" = "$ONE NIBBLE_REPO_URL=file://$UPSTREAM NIBBLE_SRC_DIR=$WORK/custom/src bash -s -- --ref v1 --rebuild" ] \
    || fail "pinned direct-run hint: $(field rebuild)"
pass "a managed checkout run directly hints at its own source and location"

echo "all $PASSED install tests passed"
