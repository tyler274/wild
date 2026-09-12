//#LinkArgs: -T tests/sources/elf/script-reverse/script-reverse.ld
//#Object:runtime.c
//#Object:ptr_black_box.c
//#Object:script-reverse-2.c
//#Object:script-reverse-3.c
//#ReferenceLinkers:bfd
//#DiffIgnore:segment.LOAD.RX.alignment

#include "../common/ptr_black_box.h"
#include "../common/runtime.h"

extern int func_aaa(void);
extern int func_bbb(void);
extern int func_ccc(void);

extern int prio_100;
extern int prio_200;
extern int prio_300;

__attribute__((used, section(".text.rev.aaa"))) int func_aaa(void) { return 1; }

int prio_100 __attribute__((section(".init_array.100"))) = 1;

void _start(void) {
  runtime_init();
  if (ptr_to_int(&func_ccc) >= ptr_to_int(&func_bbb)) {
    exit_syscall(101);
  }
  if (ptr_to_int(&func_bbb) >= ptr_to_int(&func_aaa)) {
    exit_syscall(102);
  }
  if (ptr_to_int(&prio_300) >= ptr_to_int(&prio_200)) {
    exit_syscall(103);
  }
  if (ptr_to_int(&prio_200) >= ptr_to_int(&prio_100)) {
    exit_syscall(104);
  }
  exit_syscall(42);
}
