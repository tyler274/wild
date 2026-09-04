//#RunEnabled:false
//#LinkArgs: -T tests/sources/elf/linker-script-include/linker-script-include.ld
//#Object:runtime.c
//#ReferenceLinkers:bfd
//#ExpectSym:included_sym

#include "../common/runtime.h"

void _start(void) {
  runtime_init();
  exit_syscall(42);
}
