//#AbstractConfig:base
//#Object:runtime.c
//#LinkArgs:-nostdlib -znow
//#WildExtraLinkArgs:--incremental
//#TestIncremental:true
//#DiffEnabled:false

//#Config:default:base

//#Config:opt2:base
//#CompArgs:-O2

//#Config:opt3:base
//#CompArgs:-O3

//#Config:ccache:base
//#CompilerWrapper:ccache

//#Config:lto:base
//#RequiresLinkerPlugin:true
//#LinkerDriver:gcc
//#CompArgs:-flto -O1
//#LinkArgs:-flto -O1 -nostdlib -Wl,-z,now
//#WildExtraLinkArgs:-Wl,--incremental
//#IncrementalAllowFallback:true
//#SkipArch:ppc64le

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
