// GNU `SORT_BY_NAME(EXCLUDE_FILE(...) pattern)` excludes files from that
// sorted matcher. `.sorted.bbb` would sort between aaa and ccc if kept.

//#Object:runtime.c
//#Object:script-sort-exclude-2.c
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./script-sort-exclude.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le
//#ExpectSectionBytes:.keep=0x1100000000000000 0..8
//#ExpectSectionBytes:.keep=0x3300000000000000 8..16
//#ExpectSectionBytes:.drop=0x2200000000000000 0..8
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

#include "../common/runtime.h"

long sorted_aaa __attribute__((used, section(".sorted.aaa"))) = 0x11;
long sorted_ccc __attribute__((used, section(".sorted.ccc"))) = 0x33;

void _start(void) {
  runtime_init();
  exit_syscall(42);
}
