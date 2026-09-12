//#LinkArgs: -T tests/sources/elf/script-input-section-flags/script-input-section-flags.ld
//#Object:runtime.c
//#Object:script-input-section-flags-ro.s
//#Object:script-input-section-flags-rw.s
//#Arch: x86_64
//#ReferenceLinkers:bfd
//#ExpectSym:ro_val section=".ro.flags"
//#ExpectSym:rw_val section=".rw.flags"

#include "../common/runtime.h"

extern int ro_val;
extern int rw_val;

void _start(void) {
  runtime_init();
  if (ro_val != 1 || rw_val != 2) {
    exit_syscall(101);
  }
  exit_syscall(42);
}
