//#AbstractConfig:base
//#Object:runtime.c
//#LinkArgs:-nostdlib -znow
//#WildExtraLinkArgs:--incremental
//#TestIncremental:true
//#DiffEnabled:false

//#Config:default:base

//#Config:opt0:base
//#CompArgs:-O0

//#Config:opt1:base
//#CompArgs:-O1

//#Config:opt2:base
//#CompArgs:-O2

//#Config:opt3:base
//#CompArgs:-O3

//#Config:os:base
//#CompArgs:-Os

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

//#Config:clang-opt0:base
//#Compiler:clang
//#CompArgs:-O0

//#Config:clang-opt1:base
//#Compiler:clang
//#CompArgs:-O1

//#Config:clang-opt2:base
//#Compiler:clang
//#CompArgs:-O2

//#Config:clang-opt3:base
//#Compiler:clang
//#CompArgs:-O3

//#Config:clang-os:base
//#Compiler:clang
//#CompArgs:-Os

//#Config:clang-lto:base
//#RequiresLinkerPlugin:true
//#LinkerDriver:clang
//#Compiler:clang
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
