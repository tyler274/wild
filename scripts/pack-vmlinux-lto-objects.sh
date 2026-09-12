#!/bin/sh
# Pack Clang ThinLTO x86_64 vmlinux inputs + LLD oracle for WILD_LINUX_LTO_TREE.
# Usage: pack-vmlinux-lto-objects.sh [kernel-tree] [out.tar.zst]
#
# Kernel build (from a tree that already has a .config):
#   scripts/config --enable LTO_CLANG_THIN --disable LTO_NONE
#   make LLVM=1 LLVM_IAS=1 -j"$(nproc)" vmlinux
# Copy the LLD-linked vmlinux to vmlinux.unstripped before packing.
set -eu
TREE=${1:-${WILD_LINUX_LTO_TREE:?set WILD_LINUX_LTO_TREE or pass the kernel tree}}
OUT=${2:-$PWD/vmlinux-lto-objects-x86_64.tar.zst}
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
