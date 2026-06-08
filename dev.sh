#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"

need_cmd() { command -v "$1" >/dev/null 2>&1; }

# Quick health check for critical dependencies
quick_doctor() {
  local missing=()
  for cmd in docker git cargo; do
    need_cmd "$cmd" || missing+=("$cmd")
  done
  if [[ ${#missing[@]} -gt 0 ]]; then
    echo "⚠️  Missing recommended tools: ${missing[*]}"
    echo "   Run './dev.sh doctor' for details."
    echo
  fi
}

ensure_rust() {
  if need_cmd cargo && need_cmd rustc; then
    return
  fi

  echo "Rust toolchain not found. Installing rustup..."
  if ! need_cmd curl; then
    echo "Error: curl is required to install rustup." >&2
    exit 1
  fi

  curl -fsSL https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  if [[ -f "$HOME/.cargo/env" ]]; then
    source "$HOME/.cargo/env"
  fi

  if ! need_cmd cargo; then
    echo "Error: cargo not found after rustup install. Restart your terminal and try again." >&2
    exit 1
  fi
}

ensure_rust

# Show quick doctor check only for interactive mode (no args)
if [[ $# -eq 0 ]]; then
  quick_doctor
fi

export REPO_ROOT

# Build once and reuse for faster startup
DEV_CLI_DIR="$REPO_ROOT/dev/cli"
DEV_CLI_BIN="$DEV_CLI_DIR/target/release/dev-cli"

needs_rebuild() {
  [[ ! -f "$DEV_CLI_BIN" ]] && return 0
  # Rebuild if any source file is newer than binary
  find "$DEV_CLI_DIR/src" -name '*.rs' -newer "$DEV_CLI_BIN" 2>/dev/null | grep -q . && return 0
  [[ "$DEV_CLI_DIR/Cargo.toml" -nt "$DEV_CLI_BIN" ]] && return 0
  return 1
}

if needs_rebuild; then
  echo "Building dev-cli (release mode)..."
  cargo build --release --manifest-path "$DEV_CLI_DIR/Cargo.toml" --quiet
fi

exec "$DEV_CLI_BIN" "$@"
