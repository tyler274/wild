#!/bin/sh
# Compare a Wild dirty incremental relink against other linkers' full relinks.
# Usage: bench-incremental.sh <run-with-dir> [object-to-dirty]
#
# `run-with-dir` is a WILD_SAVE_BASE capture (see BENCHMARKING.md). The optional
# object is one of that link's inputs; default is the last regular file listed
# in the save-dir's `run-with` that looks like an .o.
set -eu
DIR=${1:?usage: bench-incremental.sh <run-with-dir> [object-to-dirty]}
RUNWITH=$DIR/run-with
if [ ! -x "$RUNWITH" ] && [ ! -f "$RUNWITH" ]; then
    echo "missing $RUNWITH (capture a link with WILD_SAVE_BASE)" >&2
    exit 1
fi
if ! command -v hyperfine >/dev/null 2>&1; then
    echo "hyperfine is required (nix develop provides it)" >&2
    exit 1
fi

WILD=${WILD:-wild}
DIRTY=${2:-}
if [ -z "$DIRTY" ]; then
    DIRTY=$(awk '/\.o[" ]/{print $NF}' "$RUNWITH" | tr -d '"' | tail -n 1 || true)
fi
if [ -z "$DIRTY" ] || [ ! -f "$DIRTY" ]; then
    echo "pass an object path to dirty as argv2" >&2
    exit 1
fi

ORIG=$(mktemp)
cp -a "$DIRTY" "$ORIG"
restore() { cp -a "$ORIG" "$DIRTY"; rm -f "$ORIG"; }
trap restore EXIT

# Warm the incremental state with an unchanged Wild link, then dirty one object.
"$RUNWITH" "$WILD" --incremental
printf '\x5a' | dd of="$DIRTY" bs=1 seek=$(($(wc -c <"$DIRTY") - 1)) conv=notrunc status=none

LINKS="'$RUNWITH $WILD --incremental'"
for linker in ld ld.lld mold; do
    if command -v "$linker" >/dev/null 2>&1; then
        LINKS="$LINKS '$RUNWITH $linker'"
    fi
done
# shellcheck disable=SC2086
eval hyperfine --warmup 1 $LINKS
