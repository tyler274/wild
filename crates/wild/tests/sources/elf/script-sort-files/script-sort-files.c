// GNU `SORT(*)(.sec)` / `REVERSE(*)(.sec)` sort by input filename.
// Object order is mmm, zzz, aaa. Name order is aaa, mmm, zzz.

//#Object:runtime.c
//#Object:script-sort-files-mmm.c
//#Object:script-sort-files-zzz.c
//#Object:script-sort-files-aaa.c
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./script-sort-files.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le
//#ExpectSectionBytes:.sort=0x1100000000000000 0..8
//#ExpectSectionBytes:.sort=0x2200000000000000 8..16
//#ExpectSectionBytes:.sort=0x3300000000000000 16..24
//#ExpectSectionBytes:.rev=0x3300000000000000 0..8
//#ExpectSectionBytes:.rev=0x2200000000000000 8..16
//#ExpectSectionBytes:.rev=0x1100000000000000 16..24
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

#include "../common/runtime.h"

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
