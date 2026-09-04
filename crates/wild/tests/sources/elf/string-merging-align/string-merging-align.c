// Merge SHF_MERGE|SHF_STRINGS inputs with alignment > 1 (GCC .rodata.str1.8).
// Identical 8-aligned strings from two objects must share an address that is
// 8-aligned. The same bytes from an align-1 merge section must not be deduped
// into that pool.

//#AbstractConfig:default
//#LinkArgs:-z noexecstack
//#Object:string_merging_align1.s
//#Object:string_merging_align2.s
//#Object:runtime.c

//#Config:merge_aligned:default

#include "../common/runtime.h"

extern const char s8a[];
extern const char s8b[];
extern const char s1a[];
extern const char s1b[];

void _start(void) {
  runtime_init();

  if (s8a != s8b) {
    exit_syscall(101);
  }
  if (((unsigned long)s8a & 7) != 0) {
    exit_syscall(102);
  }
  if (s1a != s1b) {
    exit_syscall(103);
  }
  if (s8a == s1a) {
    exit_syscall(104);
  }
  if (s8a[0] != 'H' || s8a[5] != 0) {
    exit_syscall(105);
  }
  if (s1a[0] != 'H' || s1a[5] != 0) {
    exit_syscall(106);
  }
  exit_syscall(42);
}
