//#LinkArgs: -T tests/sources/elf/exclude-file/exclude-file.ld
//#Object:runtime.c
//#Object:exclude-file-keep.c
//#Object:exclude-file-drop.c
//#ReferenceLinkers:bfd
//#ExpectSym:kept_fn section=".text.keep"
//#ExpectSym:dropped_fn section=".text"

#include "../common/runtime.h"

extern int kept_fn(void);
extern int dropped_fn(void);

void _start(void) {
  runtime_init();
  exit_syscall(kept_fn() + dropped_fn());
}
