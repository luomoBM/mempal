#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

show_help() {
  cat <<EOF
Install the local mempal checkout into ~/.cargo/bin.

Usage:
  ./install-local.sh [cargo-install-options...]

Examples:
  ./install-local.sh
  ./install-local.sh --features rest
  ./install-local.sh --debug

Equivalent command:
  cargo install --path "$SCRIPT_DIR" --locked --force [options...]
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  show_help
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found in PATH" >&2
  exit 1
fi

echo "Installing mempal from: $SCRIPT_DIR"

exec cargo install --path "$SCRIPT_DIR" --locked --force "$@"
