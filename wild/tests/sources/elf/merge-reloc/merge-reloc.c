// SHF_MERGE constant pools with relocations must not be unique'd. The file
// bytes are often zeros (reloc addend) so merging would collapse distinct entries.

//#AbstractConfig:default
//#LinkArgs:-z noexecstack
//#Object:merge_reloc1.s
//#Object:merge_reloc2.s
//#Object:runtime.c
//#Arch: x86_64

//#Config:keep_reloc_merge:default

#include "../common/runtime.h"

extern const unsigned long rec1;
extern const unsigned long rec2;
extern char t1;
extern char t2;

void _start(void) {
  runtime_init();

  if (&rec1 == &rec2) {
    exit_syscall(101);
  }
  if (rec1 != (unsigned long)&t1) {
    exit_syscall(102);
  }
  if (rec2 != (unsigned long)&t2) {
    exit_syscall(103);
  }
  exit_syscall(42);
}
