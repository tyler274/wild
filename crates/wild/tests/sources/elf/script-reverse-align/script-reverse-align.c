// GNU `SORT_BY_ALIGNMENT(REVERSE(...))` is accepted but does not reverse
// alignment order (same as `SORT_BY_ALIGNMENT`: largest `sh_addralign` first).
// `REVERSE(SORT_BY_ALIGNMENT(...))` is a GNU syntax error.

//#Object:runtime.c
//#Object:ptr_black_box.c
//#LinkerScript:script-reverse-align.ld
//#ReferenceLinkers:bfd
//#DiffIgnore:segment.LOAD.RX.alignment

#include "../common/ptr_black_box.h"
#include "../common/runtime.h"

extern int align_16(void);
extern int align_64(void);
extern int align_4(void);
extern int align_32(void);

__attribute__((used, aligned(16), section(".text.align.16"))) int align_16(void) {
  return 16;
}
__attribute__((used, aligned(64), section(".text.align.64"))) int align_64(void) {
  return 64;
}
__attribute__((used, aligned(4), section(".text.align.4"))) int align_4(void) {
  return 4;
}
__attribute__((used, aligned(32), section(".text.align.32"))) int align_32(void) {
  return 32;
}

void _start(void) {
  runtime_init();
  if (ptr_to_int(&align_64) >= ptr_to_int(&align_32)) {
    exit_syscall(101);
  }
  if (ptr_to_int(&align_32) >= ptr_to_int(&align_16)) {
    exit_syscall(102);
  }
  if (ptr_to_int(&align_16) >= ptr_to_int(&align_4)) {
    exit_syscall(103);
  }
  exit_syscall(42);
}
