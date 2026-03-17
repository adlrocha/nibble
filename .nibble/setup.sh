#!/usr/bin/env bash
# .nibble/setup.sh — run once at sandbox spawn to pre-install project dependencies.
# This script is idempotent: safe to re-run if the container is restarted.
set -euo pipefail

echo "[setup] Starting nibble sandbox setup..."

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
