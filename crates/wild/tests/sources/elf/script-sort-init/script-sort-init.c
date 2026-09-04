//#LinkArgs: -T tests/sources/elf/script-sort-init/script-sort-init.ld
//#Object:runtime.c
//#Object:ptr_black_box.c
//#Object:script-sort-init-a.c
//#Object:script-sort-init-b.c
//#ReferenceLinkers:bfd
//#DiffIgnore:segment.LOAD.RX.alignment

#include "../common/ptr_black_box.h"
#include "../common/runtime.h"

extern int prio_100;
extern int prio_200;
extern int prio_65535;

void _start(void) {
  runtime_init();
  if (ptr_to_int(&prio_100) >= ptr_to_int(&prio_200)) {
    exit_syscall(101);
  }
  if (ptr_to_int(&prio_200) >= ptr_to_int(&prio_65535)) {
    exit_syscall(102);
  }
  exit_syscall(42);
}
