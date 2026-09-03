#!/bin/sh
# Pack the x86_64 vmlinux link inputs + GNU oracle for WILD_LINUX_TREE tests.
# Usage: pack-vmlinux-objects.sh [kernel-tree] [out.tar.zst]
set -eu
TREE=${1:-${WILD_LINUX_TREE:?set WILD_LINUX_TREE or pass the kernel tree}}
OUT=${2:-$PWD/vmlinux-objects-x86_64.tar.zst}
case $OUT in
    /*) ;;
    *) OUT=$PWD/$OUT ;;
esac
cd "$TREE"
for f in \
    vmlinux.o \
    .vmlinux.export.o \
    init/version-timestamp.o \
    .tmp_vmlinux2.kallsyms.o \
    arch/x86/kernel/vmlinux.lds \
    vmlinux.unstripped
do
    if [ ! -f "$f" ]; then
        echo "missing $TREE/$f" >&2
        exit 1
    fi
done
tar --zstd -cf "$OUT" \
    vmlinux.o \
    .vmlinux.export.o \
    init/version-timestamp.o \
    .tmp_vmlinux2.kallsyms.o \
    arch/x86/kernel/vmlinux.lds \
    vmlinux.unstripped
echo "Wrote $OUT"
