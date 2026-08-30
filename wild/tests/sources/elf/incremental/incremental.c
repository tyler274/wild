//#Object:runtime.c
//#WildExtraLinkArgs:--incremental
//#TestIncremental:true
//#DiffEnabled:false
//#DiffEnabled:false

#include "../common/runtime.h"

void _start(void) {
  runtime_init();
#ifdef WILD_INC
  exit_syscall(43);
#else
  exit_syscall(42);
#endif
}
