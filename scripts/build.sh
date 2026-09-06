#!/usr/bin/env bash
#
# Build and verify. Same interface across every crate in the family.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

skip_checks=false
release=false

usage() {
    cat <<USAGE
Usage: ${0##*/} [--release] [--skip-checks]

  --release      Also build an optimised binary.
  --skip-checks  Skip fmt, clippy and tests. Not recommended.
  --help         Show this message.

Checks run: cargo fmt --check, cargo clippy -D warnings (max 100 lines per
function), cargo test (includes the 1000-line-per-file limit).
USAGE
}

for arg in "$@"; do
    case "$arg" in
        --release) release=true ;;
        --skip-checks) skip_checks=true ;;
        --help | -h) usage; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

cd "$PROJECT_DIR"

if [ "$skip_checks" = false ]; then
    echo "==> fmt"
    cargo fmt --all --check

    echo "==> clippy"
    cargo clippy --all-targets --locked -- -D warnings

    echo "==> tests"
    cargo test --locked
fi

if [ "$release" = true ]; then
    echo "==> release build"
    cargo build --release --locked
fi

echo "==> ok"
