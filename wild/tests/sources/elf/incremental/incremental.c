//#Object:runtime.c
//#LinkArgs:-nostdlib -znow
//#WildExtraLinkArgs:--incremental
//#TestIncremental:true
//#DiffEnabled:false

#include "../common/runtime.h"

#ifdef WILD_INC
#define MARKER 2
#else
#define MARKER 1
#endif

int wild_inc_marker = MARKER;

void _start(void) {
  runtime_init();
  if (wild_inc_marker != MARKER) {
    exit_syscall(101);
  }
#ifdef WILD_INC
  exit_syscall(43);
#else
  exit_syscall(42);
#endif
}
