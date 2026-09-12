#!/bin/sh
# Prove wild-util predicates with Kani.
# Not packaged in nixpkgs; install with:
#   cargo install --locked kani-verifier && cargo kani setup
# CI uses model-checking/kani-github-action. Locally this no-ops if cargo-kani
# is missing (unless CI=true).
set -eu
if ! command -v cargo-kani >/dev/null 2>&1 && ! cargo kani --version >/dev/null 2>&1; then
    if [ "${CI:-}" = "true" ]; then
        echo "cargo-kani is required in CI" >&2
        exit 1
    fi
    echo "cargo-kani not installed; skipping Kani proofs" >&2
    echo "  cargo install --locked kani-verifier && cargo kani setup" >&2
    exit 0
fi
exec cargo kani -p wild-util
