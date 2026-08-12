#!/bin/bash
# install.sh — build and install nibble end-to-end
#
# Usage:
#   ./install.sh                    # full install / upgrade
#   ./install.sh --telegram         # also (re)run Telegram bot setup
#   ./install.sh --recover backup.zip  # install fresh then restore from backup

set -e

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
WRAPPERS_DIR="$HOME/.nibble/wrappers"
CLAUDE_SETTINGS="$HOME/.claude/settings.json"

step() { echo -e "\n${BOLD}▶ $1${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn() { echo -e "  ${YELLOW}!${NC} $1"; }
die()  { echo -e "  ${RED}✗${NC} $1" >&2; exit 1; }

# ── Parse flags ───────────────────────────────────────────────────────────────
RUN_TELEGRAM=false
RUN_LISTEN=false
RUN_LLAMA=false
RUN_BASELIGHT=false
RUN_PRIVACY_PROXY=false
RUN_BROWSER=false
REBUILD_IMAGE=false
RECOVER_ZIP=""

while [ $# -gt 0 ]; do
    case "$1" in
        --telegram)   RUN_TELEGRAM=true; shift ;;
        --listen)     RUN_LISTEN=true; shift ;;
        --llama)      RUN_LLAMA=true; shift ;;
        --baselight)  RUN_BASELIGHT=true; shift ;;
        --privacy-proxy) RUN_PRIVACY_PROXY=true; shift ;;
        --browser)    RUN_BROWSER=true; shift ;;
        --rebuild)    REBUILD_IMAGE=true; shift ;;
        --recover)
            if [ -z "${2:-}" ] || [ "${2#-}" != "$2" ]; then
                die "--recover requires a path to a backup zip file"
            fi
            RECOVER_ZIP="$2"
            shift 2
            ;;
        *) die "Unknown argument: $1" ;;
    esac
done

echo -e "${BOLD}=== Nibble — Install / Upgrade ===${NC}"
echo ""
echo "  Flags: --telegram   set up Telegram notifications"
echo "         --listen     set up Telegram reply listener daemon"
echo "         --llama      set up llama-server systemd service"
echo "         --baselight      install Baselight MCP server in Claude Code"
echo "         --privacy-proxy  install LLM privacy filter proxy service"
echo "         --browser        set up Chromium CDP integration for agents"
echo "         --rebuild        force rebuild the sandbox container image"
echo "         --recover        restore from a backup zip after install"
echo ""
[ "$REBUILD_IMAGE" = true ] && echo -e "  ${YELLOW}--rebuild${NC}: sandbox image will be rebuilt from scratch"
if [ -n "$RECOVER_ZIP" ]; then
    echo -e "  ${YELLOW}--recover${NC}: will restore from $RECOVER_ZIP after install"
fi

# ── 1. Prerequisites ──────────────────────────────────────────────────────────
step "Checking prerequisites"

# ---- C compiler / linker -----------------------------------------------------
if command -v cc >/dev/null 2>&1; then
    ok "cc ($(cc --version | head -n1))"
else
    warn "cc (C compiler / linker) not found — attempting to install..."
    if [ "$(uname -s)" = "Darwin" ]; then
        die "cc not found. Install Xcode Command Line Tools:  xcode-select --install"
    elif command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -qq && sudo apt-get install -y build-essential \
            || die "Failed to install build-essential via apt-get."
    elif command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y gcc gcc-c++ make \
            || die "Failed to install gcc via dnf."
    elif command -v pacman >/dev/null 2>&1; then
        sudo pacman -S --noconfirm base-devel \
            || die "Failed to install base-devel via pacman."
    else
        die "Cannot auto-install cc. Install a C compiler/toolchain manually."
    fi
    ok "cc installed"
fi

# ---- Rust / cargo ------------------------------------------------------------
# Source rustup env if cargo is not yet on PATH (common in non-login shells
# inside sandboxes where ~/.cargo/bin isn't added automatically).
if ! command -v cargo >/dev/null 2>&1; then
    for _rustup_env in \
        "$HOME/.cargo/env" \
        "$HOME/.nibble/cache/rustup/env" \
        "$HOME/.rustup/env"
    do
        # shellcheck source=/dev/null
        [ -f "$_rustup_env" ] && source "$_rustup_env" && break
    done
    # Also try well-known toolchain bin paths
    for _cargo_dir in \
        "$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin" \
        "$HOME/.nibble/cache/rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin"
    do
        [ -x "$_cargo_dir/cargo" ] && export PATH="$_cargo_dir:$PATH" && break
    done
fi

if ! command -v cargo >/dev/null 2>&1; then
    warn "cargo not found — installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # Re-source so cargo is available in the current shell
    # shellcheck source=/dev/null
    [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
    for _cargo_dir in \
        "$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin" \
        "$HOME/.nibble/cache/rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin"
    do
        [ -x "$_cargo_dir/cargo" ] && export PATH="$_cargo_dir:$PATH" && break
    done
fi
command -v cargo >/dev/null 2>&1 || die "cargo still not found after installation. Open a new terminal or re-source ~/.cargo/env and try again."
ok "cargo ($(cargo --version))"

# ---- jq ----------------------------------------------------------------------
if command -v jq >/dev/null 2>&1; then
    ok "jq found"
else
    warn "jq not found — attempting to install..."
    if [ "$(uname -s)" = "Darwin" ]; then
        if command -v brew >/dev/null 2>&1; then
            brew install jq || die "Failed to install jq via brew."
        else
            die "jq not found and Homebrew not available. Install jq manually: https://jqlang.github.io/jq/download/"
        fi
    elif command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -qq && sudo apt-get install -y jq \
            || die "Failed to install jq via apt-get."
    elif command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y jq \
            || die "Failed to install jq via dnf."
    elif command -v pacman >/dev/null 2>&1; then
        sudo pacman -S --noconfirm jq \
            || die "Failed to install jq via pacman."
    else
        die "Cannot auto-install jq. Visit https://jqlang.github.io/jq/download/"
    fi
    ok "jq installed"
fi

# ── Podman (required for sandbox) ─────────────────────────────────────────────
if command -v podman >/dev/null 2>&1; then
    ok "podman ($(podman --version))"
else
    warn "podman not found — attempting to install…"
    if command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -qq && sudo apt-get install -y podman \
            || die "Failed to install podman via apt-get. Install manually: https://podman.io/docs/installation"
    elif command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y podman \
            || die "Failed to install podman via dnf."
    elif command -v pacman >/dev/null 2>&1; then
        sudo pacman -S --noconfirm podman \
            || die "Failed to install podman via pacman."
    elif command -v brew >/dev/null 2>&1; then
        brew install podman \
            || die "Failed to install podman via brew."
    else
        die "Cannot auto-install podman. Visit https://podman.io/docs/installation"
    fi
    ok "podman installed ($(podman --version))"
fi

# Warn if podman is not in rootless mode (not a hard failure)
PODMAN_ROOTLESS=$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null || echo "unknown")
if [ "$PODMAN_ROOTLESS" = "true" ]; then
    ok "podman running rootless (good)"
else
    warn "podman is NOT rootless. For better security configure rootless mode:"
    warn "  https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md"
fi

mkdir -p "$BIN_DIR" "$WRAPPERS_DIR"

# ── 2. Build ──────────────────────────────────────────────────────────────────
step "Building release binaries"

cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" \
    || die "Cargo build failed"

ok "Build succeeded (host binary)"

# Also build a statically linked musl binary for use inside sandbox containers.
# The host binary is linked against the host glibc which may not be available
# inside the container (e.g. Arch host vs Debian container).
if [ "$(uname -s)" = "Darwin" ]; then
    warn "musl static build is not supported on macOS — skipping nibble-musl"
    warn "Container hooks will rely on the host binary (this may work if glibc matches)."
elif command -v musl-gcc >/dev/null 2>&1; then
    RUSTFLAGS="-C target-feature=+crt-static" \
        cargo build --release \
            --manifest-path "$REPO_DIR/Cargo.toml" \
            --target x86_64-unknown-linux-musl \
        && cp "$REPO_DIR/target/x86_64-unknown-linux-musl/release/nibble" \
              "$BIN_DIR/nibble-musl.new" \
        && chmod +x "$BIN_DIR/nibble-musl.new" \
        && mv -f "$BIN_DIR/nibble-musl.new" "$BIN_DIR/nibble-musl" \
        && ok "nibble-musl (static, for containers)" \
        || warn "musl build failed — container hooks won't send Telegram notifications"
else
    warn "musl-gcc not found — skipping static build (install: sudo pacman -S musl)"
    warn "Container hooks won't send Telegram notifications until this is built."
    warn "After installing musl, re-run: ./install.sh"
fi

# ── 3. Install binaries ───────────────────────────────────────────────────────
step "Installing binaries to $BIN_DIR"

# Stop the listener service before overwriting the binary (avoids "Text file busy").
LISTENER_WAS_ACTIVE=false
if systemctl --user is-active --quiet nibble-listener.service 2>/dev/null; then
    LISTENER_WAS_ACTIVE=true
    # systemctl stop can hang for 90s if the process ignores SIGTERM.
    # Use timeout (if available) or --no-block + SIGKILL to avoid hanging.
    if command -v timeout >/dev/null 2>&1; then
        timeout 5 systemctl --user stop nibble-listener.service 2>/dev/null || true
    else
        systemctl --user stop --no-block nibble-listener.service 2>/dev/null || true
        sleep 2
        systemctl --user kill --signal=SIGKILL nibble-listener.service 2>/dev/null || true
    fi
    ok "Stopped nibble-listener.service for upgrade"
fi

# Also stop the privacy proxy so we can overwrite the binary if it's running.
if systemctl --user is-active --quiet nibble-privacy-proxy.service 2>/dev/null; then
    if command -v timeout >/dev/null 2>&1; then
        timeout 5 systemctl --user stop nibble-privacy-proxy.service 2>/dev/null || true
    else
        systemctl --user stop --no-block nibble-privacy-proxy.service 2>/dev/null || true
        sleep 1
        systemctl --user kill --signal=SIGKILL nibble-privacy-proxy.service 2>/dev/null || true
    fi
    ok "Stopped nibble-privacy-proxy.service for upgrade"
fi

# Also stop the web UI so we can overwrite the binary if it's running.
WEB_WAS_ACTIVE=false
if systemctl --user is-active --quiet nibble-web.service 2>/dev/null; then
    WEB_WAS_ACTIVE=true
    if command -v timeout >/dev/null 2>&1; then
        timeout 5 systemctl --user stop nibble-web.service 2>/dev/null || true
    else
        systemctl --user stop --no-block nibble-web.service 2>/dev/null || true
        sleep 1
        systemctl --user kill --signal=SIGKILL nibble-web.service 2>/dev/null || true
    fi
    ok "Stopped nibble-web.service for upgrade"
fi

cp "$REPO_DIR/target/release/nibble" "$BIN_DIR/nibble.new"
chmod +x "$BIN_DIR/nibble.new"
mv -f "$BIN_DIR/nibble.new" "$BIN_DIR/nibble"
ok "nibble"


# Restart services that were running before.
if [ "$LISTENER_WAS_ACTIVE" = true ]; then
    systemctl --user start nibble-listener.service 2>/dev/null || warn "Could not restart nibble-listener.service"
    ok "Restarted nibble-listener.service"
fi

# Warn if BIN_DIR is not on PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$BIN_DIR"; then
    warn "$BIN_DIR is not in your PATH. Add to your ~/.zshrc or ~/.bashrc:"
    warn "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# ── 4. Install wrappers ───────────────────────────────────────────────────────
step "Installing wrappers to $WRAPPERS_DIR"

cp "$REPO_DIR/wrappers/claude-wrapper" "$WRAPPERS_DIR/claude-wrapper"
chmod +x "$WRAPPERS_DIR/claude-wrapper"
ok "claude-wrapper"

# ── 4a. Install AI Factory skills ─────────────────────────────────────────────
# Skills are installed to ~/.claude/skills/ (Claude Code),
# ~/.nibble/skills/ (internal), and ~/.pi/agent/skills/ (Pi harness).
# Existing files are overwritten so updates always propagate.
install_skill() {
    local src_dir="$1"
    local dest_dir="$2"
    local skill_name
    skill_name="$(basename "$src_dir")"
    local dest="$dest_dir/$skill_name"
    # Copy the whole skill directory (SKILL.md plus references/, scripts/, assets/)
    # so on-demand reference files and helper scripts ship to sandboxes.
    rm -rf "$dest"
    cp -r "$src_dir" "$dest"
    ok "skill: $skill_name → $dest/"
}

CLAUDE_SKILLS_DIR="$HOME/.claude/skills"
NIBBLE_SKILLS_DIR="$HOME/.nibble/skills"
PI_SKILLS_DIR="$HOME/.pi/agent/skills"
mkdir -p "$CLAUDE_SKILLS_DIR" "$NIBBLE_SKILLS_DIR"

for skill_dir in "$REPO_DIR/skills"/*/; do
    [ -d "$skill_dir" ] || continue
    install_skill "$skill_dir" "$CLAUDE_SKILLS_DIR"
    install_skill "$skill_dir" "$NIBBLE_SKILLS_DIR"
    if [ -d "$PI_SKILLS_DIR" ] || [ -d "$HOME/.pi" ]; then
        mkdir -p "$PI_SKILLS_DIR"
        install_skill "$skill_dir" "$PI_SKILLS_DIR"
    fi
done

# Remove factory stage skills that were consolidated into factory-pipeline/.
# Targeted (not a blanket purge) so third-party skills in ~/.claude/skills are kept.
for stale in factory-spec factory-verify factory-qa-gate factory-lessons; do
    for dest in "$CLAUDE_SKILLS_DIR" "$NIBBLE_SKILLS_DIR" "$PI_SKILLS_DIR"; do
        [ -d "$dest/$stale" ] && rm -rf "$dest/$stale" && ok "removed stale skill: $stale"
    done
done
if [ ! -d "$HOME/.pi" ]; then
    warn "~/.pi/ not found — skills staged in ~/.nibble/skills/ only"
fi

# ── 4b. Install Pi extensions ─────────────────────────────────────────────────
# Nibble-managed Pi extensions live in pi-extensions/ and are installed to
# ~/.pi/agent/extensions/ and ~/.nibble/extensions/ for use by agents.
NIBBLE_EXT_DIR="$HOME/.nibble/extensions"
PI_EXT_DIR="$HOME/.pi/agent/extensions"
mkdir -p "$NIBBLE_EXT_DIR"
for ext_file in "$REPO_DIR/pi-extensions/"*.ts; do
    [ -f "$ext_file" ] || continue
    ext_name="$(basename "$ext_file")"
    cp "$ext_file" "$NIBBLE_EXT_DIR/$ext_name"
    ok "pi extension: $ext_name → ~/.nibble/extensions/"
    if [ -d "$PI_EXT_DIR" ] || [ -d "$HOME/.pi" ]; then
        mkdir -p "$PI_EXT_DIR"
        if [ -L "$PI_EXT_DIR/$ext_name" ]; then
            rm -f "$PI_EXT_DIR/$ext_name"
        fi
        cp "$ext_file" "$PI_EXT_DIR/$ext_name"
        ok "pi extension: $ext_name → ~/.pi/agent/extensions/ (copy)"
    fi
done
if [ ! -d "$HOME/.pi" ]; then
    warn "~/.pi/ not found — extensions staged in ~/.nibble/extensions/ only"
    warn "  (will be installed automatically when you spawn a Pi sandbox)"
fi

# ── 4b-ext. External Pi packages (npm/git) ───────────────────────────────────
# Same delivery model as the local *.ts extensions above (host ~/.pi/agent is
# bind-mounted into every sandbox), but these need `pi install`. Source of
# truth: pi-extensions/external-packages.txt. nibble also installs these
# in-container at spawn as a fallback (see [pi].extensions in config.toml).
EXTERNAL_MANIFEST="$REPO_DIR/pi-extensions/external-packages.txt"
if [ -f "$EXTERNAL_MANIFEST" ]; then
    if command -v pi >/dev/null 2>&1; then
        while IFS= read -r line || [ -n "$line" ]; do
            pkg="$(printf '%s' "${line%%#*}" | tr -d '[:space:]')"
            [ -z "$pkg" ] && continue
            if pi install "$pkg" >/dev/null 2>&1; then
                ok "pi package (host): $pkg → ~/.pi/agent"
            else
                warn "pi package (host): $pkg install failed (will retry at spawn)"
            fi
        done < "$EXTERNAL_MANIFEST"
    else
        warn "\`pi\` not on host PATH — external pi packages will install at spawn time"
    fi
fi

# ── 4c. Install Claude Code statusline ────────────────────────────────────────
# Always copy the script; only add the statusLine key to settings.json if one
# is not already configured (so custom statuslines aren't clobbered).
STATUSLINE_SCRIPT="$HOME/.claude/statusline-command.sh"
cp "$REPO_DIR/scripts/statusline-command.sh" "$STATUSLINE_SCRIPT"
chmod +x "$STATUSLINE_SCRIPT"
ok "statusline-command.sh → $STATUSLINE_SCRIPT"

if command -v jq >/dev/null 2>&1; then
    if [ -f "$CLAUDE_SETTINGS" ] && jq -e '.statusLine' "$CLAUDE_SETTINGS" >/dev/null 2>&1; then
        ok "statusLine already configured in settings.json — skipping"
    else
        # Merge statusLine into settings.json (create file if absent)
        STATUS_JSON=$(jq -n --arg cmd "bash \$HOME/.claude/statusline-command.sh" \
            '{statusLine: {type: "command", command: $cmd}}')
        if [ -f "$CLAUDE_SETTINGS" ]; then
            jq -s '.[0] * .[1]' "$CLAUDE_SETTINGS" <(echo "$STATUS_JSON") > "$CLAUDE_SETTINGS.tmp" \
                && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
        else
            echo "$STATUS_JSON" > "$CLAUDE_SETTINGS"
        fi
        ok "statusLine configured in settings.json"
    fi
else
    warn "jq not found — could not configure statusLine in settings.json"
    warn "Add manually: { \"statusLine\": { \"type\": \"command\", \"command\": \"bash \$HOME/.claude/statusline-command.sh\" } }"
fi

# Check shell aliases
SHELL_RC=""
[ -f "$HOME/.zshrc" ]  && SHELL_RC="$HOME/.zshrc"
[ -f "$HOME/.bashrc" ] && [ -z "$SHELL_RC" ] && SHELL_RC="$HOME/.bashrc"

if [ -n "$SHELL_RC" ]; then
    MISSING_ALIASES=()
    grep -q "nibble/wrappers/claude-wrapper" "$SHELL_RC" 2>/dev/null || MISSING_ALIASES+=("claude")

    if [ ${#MISSING_ALIASES[@]} -eq 0 ]; then
        ok "Shell aliases already in $SHELL_RC"
    else
        warn "Add these aliases to $SHELL_RC and reload your shell:"
        for name in "${MISSING_ALIASES[@]}"; do
            warn "  alias ${name}='$WRAPPERS_DIR/${name}-wrapper'"
        done
        warn "  source $SHELL_RC"
    fi
else
    warn "Could not detect shell RC. Add aliases manually:"
    warn "  alias claude='$WRAPPERS_DIR/claude-wrapper'"
fi

# ── 5. Sandbox image ─────────────────────────────────────────────────────────
step "Building sandbox image"

SANDBOX_BUILD_ARGS=""
[ "$REBUILD_IMAGE" = true ] && SANDBOX_BUILD_ARGS="--rebuild"

if "$BIN_DIR/nibble" sandbox build $SANDBOX_BUILD_ARGS; then
    ok "Sandbox image ready: nibble-sandbox:latest"
else
    warn "Sandbox image build failed."
    warn "Retry with: ./install.sh --rebuild"
fi

# ── 5b. Hermes sandbox image ──────────────────────────────────────────────────
# Hermes uses a separate image (nibble-hermes:latest) with Python + Hermes Agent.
# Build it lazily on first `nibble hermes init`, or eagerly here if podman is available.
if "$BIN_DIR/nibble" sandbox build --image nibble-hermes:latest $SANDBOX_BUILD_ARGS 2>/dev/null; then
    ok "Hermes sandbox image ready: nibble-hermes:latest"
else
    ok "Hermes image will be built on first 'nibble hermes init'"
fi

# Ensure SYSTEMD_DIR is defined before any service installation sections.
SYSTEMD_DIR="$HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_DIR"

# ── 5c. Privacy filter proxy (optional) ──────────────────────────────────────
step "Installing privacy filter proxy"

mkdir -p "$HOME/.nibble"
cp "$REPO_DIR/scripts/privacy-proxy.py" "$HOME/.nibble/privacy-proxy.py"
chmod +x "$HOME/.nibble/privacy-proxy.py"
ok "privacy-proxy.py → $HOME/.nibble/privacy-proxy.py"

# Stage llama-server script + model profiles so `nibble lm use` can find them
# from the installed binary in ~/.local/bin (outside the repo tree).
cp "$REPO_DIR/scripts/setup-llama-server.sh" "$HOME/.nibble/setup-llama-server.sh"
chmod +x "$HOME/.nibble/setup-llama-server.sh"
cp "$REPO_DIR/scripts/llm-model-profiles.toml" "$HOME/.nibble/llm-model-profiles.toml"
ok "llama-server script + profiles → $HOME/.nibble/"

# Check Python + dependencies
if command -v python3 >/dev/null 2>&1; then
    ok "python3 found"
    # Try importing required packages
    if python3 -c "import fastapi, httpx, uvicorn, transformers" 2>/dev/null; then
        ok "Python dependencies installed (fastapi, httpx, uvicorn, transformers)"
    else
        warn "Missing Python dependencies for privacy proxy."
        warn "Install with:"
        warn "  python3 -m pip install --user fastapi httpx uvicorn transformers torch"
        warn ""
        warn "Or run with --privacy-proxy to attempt auto-install."
    fi
else
    warn "python3 not found — privacy proxy requires Python 3."
    warn "Install Python 3 and then run:"
    warn "  python3 -m pip install --user fastapi httpx uvicorn transformers torch"
fi

cat > "$SYSTEMD_DIR/nibble-privacy-proxy.service" << UNIT
[Unit]
Description=Nibble LLM Privacy Filter Proxy
After=network.target

[Service]
Type=simple
ExecStart=%h/.nibble/privacy-proxy.py
Restart=always
Environment=HOME=%h

[Install]
WantedBy=default.target
UNIT

if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable nibble-privacy-proxy.service 2>/dev/null || true
    if [ "$RUN_PRIVACY_PROXY" = true ]; then
        systemctl --user restart nibble-privacy-proxy.service 2>/dev/null \
            && ok "Privacy proxy service started" \
            || warn "Could not start privacy proxy service"
    else
        ok "Privacy proxy service installed (enable with --privacy-proxy)"
    fi
else
    warn "systemd user session not available. Privacy proxy won't auto-start."
    warn "Start manually: python3 $HOME/.nibble/privacy-proxy.py"
fi

# ── 5a. Install systemd auto-resume service ───────────────────────────────────
cat > "$SYSTEMD_DIR/nibble-resume.service" << UNIT
[Unit]
Description=Nibble — resume sandbox agents after reboot
After=default.target

[Service]
Type=oneshot
ExecStart=$BIN_DIR/nibble sandbox resume --all
RemainAfterExit=yes

[Install]
WantedBy=default.target
UNIT

if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable nibble-resume.service 2>/dev/null \
        && ok "Auto-resume service enabled (nibble-resume.service)" \
        || warn "Could not enable auto-resume service. Enable manually: systemctl --user enable nibble-resume.service"
else
    warn "systemd user session not available. Auto-resume on reboot won't work."
fi

# ── 5d. Token-usage tracker (systemd-user timer) ─────────────────────────────
# Scans claude + pi session logs every 15 minutes and writes per-message
# token counts into ~/.nibble/tasks.db. Query with `nibble usage report`.
step "Installing token-usage tracker timer"

cat > "$SYSTEMD_DIR/nibble-usage.service" << UNIT
[Unit]
Description=Nibble — scan Claude/pi session logs for token usage
After=default.target

[Service]
Type=oneshot
ExecStart=$BIN_DIR/nibble usage scan
Environment=HOME=%h
UNIT

cat > "$SYSTEMD_DIR/nibble-usage.timer" << UNIT
[Unit]
Description=Run nibble usage scan every 15 minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec=15min
Unit=nibble-usage.service

[Install]
WantedBy=timers.target
UNIT

if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable --now nibble-usage.timer 2>/dev/null \
        && ok "Usage scan timer enabled (every 15 min)" \
        || warn "Could not enable nibble-usage.timer. Enable manually: systemctl --user enable --now nibble-usage.timer"
else
    warn "systemd user session not available. Run scans manually: nibble usage scan"
fi

# Seed the pricing override file if it doesn't exist.
PRICING_DIR="$HOME/.nibble"
PRICING_FILE="$PRICING_DIR/pricing.toml"
if [ ! -f "$PRICING_FILE" ]; then
    mkdir -p "$PRICING_DIR"
    cat > "$PRICING_FILE" << 'PRICING'
# nibble usage pricing overrides — USD per 1M tokens.
# Bundled defaults exist for common Claude models; entries here override them.
# View the effective table with `nibble usage pricing`.
#
# Example:
# [anthropic."claude-opus-4-7"]
# input = 15.0
# output = 75.0
# cache_read = 1.50
# cache_write = 18.75
#
# Free providers (e.g. zai for glm-*) record cost=0 in pi logs by default.
# Add pricing here if you want to estimate what they'd cost on a paid plan.
PRICING
    ok "Pricing override stub written: $PRICING_FILE"
fi

# ── 5e. Web session inspector (systemd-user service) ────────────────────────
# Dark-mode browser UI for browsing/searching pi sessions, token stats and
# live tasks. Binds 0.0.0.0:7878 so it's reachable over Tailscale; a bearer
# token in ~/.nibble/web.env guards against hostile-LAN exposure.
step "Installing web session inspector"

WEB_ENV="$HOME/.nibble/web.env"
if [ ! -f "$WEB_ENV" ]; then
    if command -v openssl >/dev/null 2>&1; then
        WEB_TOKEN=$(openssl rand -hex 24)
    else
        WEB_TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
    fi
    printf 'NIBBLE_WEB_TOKEN=%s\n' "$WEB_TOKEN" > "$WEB_ENV"
    chmod 600 "$WEB_ENV"
    ok "Generated web auth token: $WEB_ENV"
fi

cat > "$SYSTEMD_DIR/nibble-web.service" << UNIT
[Unit]
Description=Nibble Web — session inspector UI (port 7878)
After=network-online.target

[Service]
Type=simple
ExecStart=$BIN_DIR/nibble web
EnvironmentFile=-%h/.nibble/web.env
Restart=on-failure

[Install]
WantedBy=default.target
UNIT

if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable nibble-web.service 2>/dev/null || true
    systemctl --user restart nibble-web.service 2>/dev/null \
        && ok "Web session inspector running (nibble-web.service)" \
        || warn "Could not start nibble-web.service"
    WEB_TOKEN=$(cut -d= -f2- "$WEB_ENV")
    TS_IP=$(tailscale ip -4 2>/dev/null | head -1 || true)
    ok "  Local:    http://localhost:7878/?token=$WEB_TOKEN"
    [ -n "$TS_IP" ] && ok "  Tailnet:  http://$TS_IP:7878/?token=$WEB_TOKEN"
else
    warn "systemd user session not available. Start manually: nibble web"
fi

# ── 6. Claude Code hooks ──────────────────────────────────────────────────────
step "Installing Claude Code hooks"

mkdir -p "$HOME/.claude"

# Pre-remove any existing nibble hooks so setup-claude-hooks.sh always
# writes the latest version.
if grep -q "AGENT_TASK_ID" "$CLAUDE_SETTINGS" 2>/dev/null; then
    if command -v jq >/dev/null 2>&1; then
        jq 'del(.hooks)' "$CLAUDE_SETTINGS" > "$CLAUDE_SETTINGS.tmp" \
            && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
    fi
fi

bash "$REPO_DIR/scripts/setup-claude-hooks.sh"

# ── 7. Memory system setup ──────────────────────────────────────────────────
step "Setting up memory system"

MEMORY_DIR="$HOME/.nibble/memory"
NEEDS_SETUP=false

# Auto-initialize if directory doesn't exist yet
if [ ! -d "$MEMORY_DIR/.git" ]; then
    if "$BIN_DIR/nibble" memory reindex 2>/dev/null; then
        ok "Memory directory created and git-initialized"
    else
        warn "Could not auto-initialize memory dir"
    fi
fi

# Check current state
if [ -d "$MEMORY_DIR/.git" ]; then
    ok "memory directory initialized ($MEMORY_DIR)"

    # Quick stats
    MEM_COUNT=$(find "$MEMORY_DIR/memories" -name '*.md' 2>/dev/null | wc -l | tr -d ' ')
    LESSON_COUNT=$(find "$MEMORY_DIR/lessons" -name '*.md' 2>/dev/null | wc -l | tr -d ' ')
    ok "$MEM_COUNT memories, $LESSON_COUNT lessons"

    # Check if remote is wired
    MEM_REMOTE=$(git -C "$MEMORY_DIR" remote 2>/dev/null | head -1)
    if [ -n "$MEM_REMOTE" ]; then
        MEM_REMOTE_URL=$(git -C "$MEMORY_DIR" remote get-url "$MEM_REMOTE" 2>/dev/null)
        ok "sync remote: $MEM_REMOTE_URL"
    else
        NEEDS_SETUP=true
    fi
else
    NEEDS_SETUP=true
fi

if [ "$NEEDS_SETUP" = true ]; then
    echo ""
    warn "Memory system needs configuration."

    # Only offer interactive wizard when stdin is a terminal
    if [ -t 0 ]; then
        echo ""
        echo -n "  Launch setup wizard? [Y/n] "
        read -r answer
        case "$answer" in
            [Nn]* | [Nn][Oo] )
                echo ""
                echo "  Skipped. Run anytime:"
                echo -e "    ${BOLD}nibble memory config --setup${NC}"
                echo ""
                ;;
            * )
                echo ""
                "$BIN_DIR/nibble" memory config --setup
                ;;
        esac
    else
        echo ""
        echo "  Run the setup wizard:"
        echo -e "    ${BOLD}nibble memory config --setup${NC}"
        echo ""
        echo "  Or clone an existing memory repo:"
        echo "    git clone <your-repo-url> ~/.nibble/memory"
        echo ""
    fi
fi

# ── 8. Telegram (optional) ────────────────────────────────────────────────────
if [ "$RUN_TELEGRAM" = true ]; then
    step "Setting up Telegram notifications"
    bash "$REPO_DIR/scripts/setup-telegram.sh"
else
    CONFIG_FILE="$HOME/.nibble/config.toml"
    if grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
        ok "Telegram already configured ($CONFIG_FILE)"
    else
        echo ""
        warn "Telegram not configured. Run when ready:"
        warn "  ./install.sh --telegram"
    fi
fi

# ── 9. Telegram listener daemon (optional) ────────────────────────────────────
if [ "$RUN_LISTEN" = true ]; then
    step "Setting up Telegram reply listener (systemd service)"
    bash "$REPO_DIR/scripts/setup-listen.sh"
else
    # Offer the hint only when Telegram is already configured but listener isn't running.
    CONFIG_FILE="$HOME/.nibble/config.toml"
    if grep -q "enabled = true" "$CONFIG_FILE" 2>/dev/null; then
        if ! systemctl --user is-active --quiet nibble-listener.service 2>/dev/null; then
            echo ""
            warn "Telegram reply listener not running. Enable with:"
            warn "  ./install.sh --listen"
        else
            ok "Telegram reply listener already running"
        fi
    fi
fi

# ── 10. Llama server (optional) ────────────────────────────────────────────────
if [ "$RUN_LLAMA" = true ]; then
    step "Setting up llama-server service"
    bash "$REPO_DIR/scripts/setup-llama-server.sh"
else
    if [ ! -f /etc/systemd/system/llama-server.service ]; then
        echo ""
        warn "llama-server not installed. Set up with:"
        warn "  ./install.sh --llama"
    fi
fi

# ── 11. Baselight MCP server (optional) ───────────────────────────────────────
if [ "$RUN_BASELIGHT" = true ]; then
    step "Installing Baselight MCP server into Claude Code settings"

    if ! command -v jq >/dev/null 2>&1; then
        die "jq is required for --baselight but was not found"
    fi

    # Resolve API key: env var > prompt
    if [ -z "${BASELIGHT_API_KEY:-}" ]; then
        echo ""
        echo "  Enter your Baselight API key (from app.baselight.ai → Settings → API Keys):"
        read -r -p "  API key: " BASELIGHT_API_KEY
        BASELIGHT_API_KEY="${BASELIGHT_API_KEY// /}"
    fi

    if [ -z "$BASELIGHT_API_KEY" ]; then
        die "No Baselight API key provided. Set BASELIGHT_API_KEY or pass it interactively."
    fi

    MCP_JSON=$(jq -n --arg key "$BASELIGHT_API_KEY" '{
        mcpServers: {
            baselight: {
                type: "http",
                url: "https://api.baselight.app/mcp",
                headers: { "x-api-key": $key }
            }
        }
    }')

    if [ -f "$CLAUDE_SETTINGS" ] && [ "$(cat "$CLAUDE_SETTINGS")" != "{}" ] && [ -s "$CLAUDE_SETTINGS" ]; then
        jq -s '.[0] * .[1]' "$CLAUDE_SETTINGS" <(echo "$MCP_JSON") > "$CLAUDE_SETTINGS.tmp" \
            && mv "$CLAUDE_SETTINGS.tmp" "$CLAUDE_SETTINGS"
    else
        echo "$MCP_JSON" > "$CLAUDE_SETTINGS"
    fi

    ok "Baselight MCP server configured in $CLAUDE_SETTINGS"
    ok "Restart Claude Code for the MCP server to appear"
else
    # Show hint if not already configured
    if [ -f "$CLAUDE_SETTINGS" ] && jq -e '.mcpServers.baselight' "$CLAUDE_SETTINGS" >/dev/null 2>&1; then
        ok "Baselight MCP already configured in settings.json"
    else
        echo ""
        warn "Baselight MCP not configured. Install with:"
        warn "  ./install.sh --baselight"
        warn "  (or set BASELIGHT_API_KEY=... ./install.sh --baselight)"
    fi
fi

# ── 12. Browser CDP integration (optional) ───────────────────────────────────
if [ "$RUN_BROWSER" = true ]; then
    step "Setting up Chromium CDP browser integration"

    # Install browser.sh to ~/.local/bin/browser
    mkdir -p "$BIN_DIR"
    cp "$REPO_DIR/scripts/browser.sh" "$BIN_DIR/browser"
    chmod +x "$BIN_DIR/browser"
    ok "browser → $BIN_DIR/browser"

    # Create persistent Chromium profile dir
    mkdir -p "$HOME/.nibble/chromium-profile"
    ok "Chromium profile dir: ~/.nibble/chromium-profile"

    # Add chromium-debug alias to shell RC
    BROWSER_ALIAS='alias chromium-debug='"'"'chromium --remote-debugging-port=9222 --user-data-dir=$HOME/.nibble/chromium-profile --no-first-run --no-default-browser-check'"'"
    SHELL_RC=""
    [ -f "$HOME/.zshrc" ]  && SHELL_RC="$HOME/.zshrc"
    [ -f "$HOME/.bashrc" ] && [ -z "$SHELL_RC" ] && SHELL_RC="$HOME/.bashrc"

    if [ -n "$SHELL_RC" ]; then
        if grep -q "chromium-debug" "$SHELL_RC" 2>/dev/null; then
            ok "chromium-debug alias already in $SHELL_RC"
        else
            echo "" >> "$SHELL_RC"
            echo "# nibble: Chromium CDP debug alias (added by install.sh --browser)" >> "$SHELL_RC"
            echo "$BROWSER_ALIAS" >> "$SHELL_RC"
            ok "chromium-debug alias added to $SHELL_RC"
            warn "Run 'source $SHELL_RC' or open a new terminal to activate the alias"
        fi
    else
        warn "Could not detect shell RC. Add this alias manually:"
        warn "  $BROWSER_ALIAS"
    fi

    # Add CDP curl permission to project settings if not present
    PROJ_SETTINGS="$REPO_DIR/.claude/settings.json"
    if [ -f "$PROJ_SETTINGS" ] && command -v jq >/dev/null 2>&1; then
        if jq -e '.permissions.allow // [] | map(select(test("localhost:9222"))) | length > 0' "$PROJ_SETTINGS" >/dev/null 2>&1; then
            ok "CDP curl permission already in .claude/settings.json"
        else
            jq '.permissions.allow += ["Bash(curl http://localhost:9222*)", "Bash(curl -s http://localhost:9222*)"]' \
                "$PROJ_SETTINGS" > "$PROJ_SETTINGS.tmp" \
                && mv "$PROJ_SETTINGS.tmp" "$PROJ_SETTINGS"
            ok "CDP curl permission added to .claude/settings.json"
        fi
    fi
else
    if [ -x "$BIN_DIR/browser" ]; then
        ok "Browser CDP integration already installed"
    else
        echo ""
        warn "Chromium CDP integration not set up. Enable with:"
        warn "  ./install.sh --browser"
    fi
fi

# ── 14. Recover from backup (optional) ────────────────────────────────────────
if [ -n "$RECOVER_ZIP" ]; then
    step "Recovering from backup"
    if [ ! -f "$RECOVER_ZIP" ]; then
        die "Backup file not found: $RECOVER_ZIP"
    fi
    "$BIN_DIR/nibble" import "$RECOVER_ZIP" \
        || die "Failed to import backup"
    ok "Restored from $RECOVER_ZIP"
fi

# ── 15. Done ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${GREEN}Done!${NC} Restart Claude Code for hooks to take effect."
echo ""
echo "  Verify:      nibble --help"
echo "  Test notify: nibble notify --message 'install test' --attention"
echo ""
echo -e "${BOLD}Sandbox usage:${NC}"
echo "  Start agent:  nibble sandbox spawn /path/to/repo"
echo "  List agents:  nibble sandbox list"
echo "  Attach:       nibble sandbox attach <task-id>"
echo "  Kill agent:   nibble sandbox kill <task-id>"
echo "  Watch:        nibble watch"
echo "  Rebuild img:  ./install.sh --rebuild"
echo ""
echo -e "${BOLD}Hermes usage:${NC}"
echo "  Start:        nibble hermes init"
echo "  Attach:       nibble hermes attach"
echo "  Mount repo:   nibble hermes mount /path/to/repo"
echo "  Unmount repo: nibble hermes unmount /path/to/repo"
echo "  List repos:   nibble hermes list"
echo "  Stop:         nibble hermes kill"
echo ""
echo -e "${BOLD}Memory usage:${NC}"
echo "  Config:       nibble memory config"
echo "  Write:        nibble memory write 'decision: chose Rust over Go'"
echo "  Search:       nibble memory search 'database decision'"
echo "  List:         nibble memory list"
echo "  Lessons:      nibble memory lessons"
echo "  Sync:         nibble memory sync"
echo ""
echo -e "${BOLD}Backup usage:${NC}"
echo "  Backup:       nibble backup"
echo "  Import:       nibble import <backup.zip>"
echo "  Recover:      ./install.sh --recover <backup.zip>"
echo ""
