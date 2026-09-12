// GNU nested SORT_BY_NAME(SORT_BY_ALIGNMENT) and SORT_BY_ALIGNMENT(SORT_BY_NAME).
// High-alignment `.nta.zzz` must not precede `.nta.aaa` when name is primary.

//#Object:runtime.c
//#Object:script-sort-nested-2.c
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./script-sort-nested.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le
//#ExpectSectionBytes:.name_then_align=0xaa00000000000000 0..8
//#ExpectSectionBytes:.name_then_align=0x1100000000000000 8..16
//#ExpectSectionBytes:.name_then_align=0x2200000000000000 16..24
//#ExpectSectionBytes:.name_then_align=0x3300000000000000 32..40
//#ExpectSectionBytes:.align_then_name=0x2200000000000000 0..8
//#ExpectSectionBytes:.align_then_name=0x1100000000000000 8..16
//#ExpectSectionBytes:.align_then_name=0x3300000000000000 16..24
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

#include "../common/runtime.h"

long nta_aaa_1 __attribute__((used, aligned(1), section(".nta.aaa"))) = 0x11;
long nta_bbb __attribute__((used, aligned(1), section(".nta.bbb"))) = 0x22;

long atn_zzz __attribute__((used, aligned(8), section(".atn.zzz"))) = 0x33;
long atn_aaa __attribute__((used, aligned(8), section(".atn.aaa"))) = 0x11;
long atn_bbb __attribute__((used, aligned(16), section(".atn.bbb"))) = 0x22;

void _start(void) {
  runtime_init();
  exit_syscall(42);
}
