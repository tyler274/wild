// Mixed `sh_addralign` in one harvest part: `--sort-section` on an unsorted
// script wildcard, and `SORT_BY_NAME`, must pad like GNU ld (not alignment
// buckets). Name-sort places `.aaa` at 0 and pads `.zzz` to 64.

//#AbstractConfig:default
//#Object:runtime.c
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment
//#ExpectSectionBytes:.byname=0x1100000000000000 0..8
//#ExpectSectionBytes:.byname=0x3300000000000000 64..72

//#Config:name:default
//#LinkArgs:-nostdlib -znow --no-gc-sections --sort-section=name -T ./script-sort-cli.ld
//#ExpectSectionBytes:.sorted=0x1100000000000000 0..8
//#ExpectSectionBytes:.sorted=0x3300000000000000 64..72

//#Config:align:default
//#LinkArgs:-nostdlib -znow --no-gc-sections --sort-section=alignment -T ./script-sort-cli.ld
//#ExpectSectionBytes:.sorted=0x3300000000000000 0..8
//#ExpectSectionBytes:.sorted=0x1100000000000000 8..16

#include "../common/runtime.h"

long sort_aaa __attribute__((used, aligned(8), section(".keep.sort.aaa"))) = 0x11;
long sort_zzz __attribute__((used, aligned(64), section(".keep.sort.zzz"))) = 0x33;

long byname_zzz __attribute__((used, aligned(64), section(".keep.byname.zzz"))) = 0x33;
long byname_aaa __attribute__((used, aligned(8), section(".keep.byname.aaa"))) = 0x11;

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
