// Merge SHF_MERGE constant pools with alignment > 1 (GCC .rodata.cst8).
// Identical 8-byte units from two objects must share an 8-aligned address.
// The same bytes from an entsize-1 merge section must not be split into that pool.

//#AbstractConfig:default
//#LinkArgs:-z noexecstack
//#Object:const_merging_align1.s
//#Object:const_merging_align2.s
//#Object:runtime.c

//#Config:merge_cst8:default

#include "../common/runtime.h"

extern const unsigned long c8a;
extern const unsigned long c8b;
extern const unsigned long c8unique;
extern const unsigned long blob;

void _start(void) {
  runtime_init();

  if (&c8a != &c8b) {
    exit_syscall(101);
  }
  if (((unsigned long)&c8a & 7) != 0) {
    exit_syscall(102);
  }
  if (c8a != 0x1122334455667788UL) {
    exit_syscall(103);
  }
  if (c8unique == c8a) {
    exit_syscall(104);
  }
  if (blob != 0x1122334455667788UL) {
    exit_syscall(105);
  }
  if (&blob == &c8a) {
    exit_syscall(106);
  }
  exit_syscall(42);
}
