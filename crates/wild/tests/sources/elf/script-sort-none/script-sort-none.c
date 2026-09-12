// GNU `SORT_NONE` keeps input order. `--sort-section` must not override it.
// Unsorted wildcards still take `--sort-section=name`.

//#Object:runtime.c
//#LinkArgs:-nostdlib -znow --no-gc-sections --sort-section=name -T ./script-sort-none.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le
//#ExpectSectionBytes:.sorted=0x1100000000000000 0..8
//#ExpectSectionBytes:.sorted=0x3300000000000000 8..16
//#ExpectSectionBytes:.none=0x3300000000000000 0..8
//#ExpectSectionBytes:.none=0x1100000000000000 8..16
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

#include "../common/runtime.h"

long none_zzz __attribute__((used, section(".keep.none.zzz"))) = 0x33;
long none_aaa __attribute__((used, section(".keep.none.aaa"))) = 0x11;

long sort_zzz __attribute__((used, section(".keep.sort.zzz"))) = 0x33;
long sort_aaa __attribute__((used, section(".keep.sort.aaa"))) = 0x11;

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
