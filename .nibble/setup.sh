#!/usr/bin/env bash
# .nibble/setup.sh — run once at sandbox spawn to pre-install project dependencies.
# This script is idempotent: safe to re-run if the container is restarted.
set -euo pipefail

echo "[setup] Starting nibble sandbox setup..."

# ── SSH known_hosts for memory auto-sync ──────────────────────────────────────
# Pin GitHub's ED25519 key to avoid interactive prompts and MITM risks from
# dynamic ssh-keyscan. Fingerprint: SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU
# Source: https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints
mkdir -p ~/.ssh
GITHUB_KEY='github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl'
if ! grep -qF "$GITHUB_KEY" ~/.ssh/known_hosts 2>/dev/null; then
    echo "[setup] Pinning GitHub SSH host key..."
    echo "$GITHUB_KEY" >> ~/.ssh/known_hosts
fi

# ── System build dependencies ─────────────────────────────────────────────────
if ! command -v cc &>/dev/null; then
    echo "[setup] Installing build-essential..."
    sudo apt-get update -qq && sudo apt-get install -y -qq build-essential
fi

# ── Rust toolchain ────────────────────────────────────────────────────────────
export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v rustup &>/dev/null; then
    echo "[setup] Installing rustup..."
    curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path
fi

# Ensure the toolchain matches rust-toolchain.toml if present
if [ -f /workspace/rust-toolchain.toml ]; then
    echo "[setup] Syncing toolchain from rust-toolchain.toml..."
    rustup show active-toolchain || true
fi

# ── Build the project ─────────────────────────────────────────────────────────
echo "[setup] Building project (cargo build)..."
cd /workspace
cargo build

echo "[setup] Done. Rust toolchain and project binary are ready."
