// GNU SORT_BY_ALIGNMENT uses input order when alignments match, not name.
// zzz is first in the file; aaa would win a name sort.

//#Object:runtime.c
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./script-sort-align-tie.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le
//#ExpectSectionBytes:.data=0x3300000000000000 0..8
//#ExpectSectionBytes:.data=0x1100000000000000 8..16
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

#include "../common/runtime.h"

long zzz __attribute__((used, aligned(8), section(".data.align.zzz"))) = 0x33;
long aaa __attribute__((used, aligned(8), section(".data.align.aaa"))) = 0x11;

void _start(void) {
    runtime_init();
    (void)zzz;
    (void)aaa;
    exit_syscall(42);
}
