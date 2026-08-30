// Checks auto-discovery of a linker plugin when --plugin is not passed.

//#AbstractConfig:default
//#RequiresLinkerPlugin:true
//#Object:runtime.c
//#CompArgs:-flto
//#LinkArgs:-nostdlib -znow
//#ReferenceLinkers:
//#DiffEnabled:false

//#Config:gcc:default
//#Compiler:gcc
//#LinkerDriver:gcc
//#LinkArgs:-flto -nostdlib -znow

//#Config:clang:default
//#Compiler:clang

#include "../common/runtime.h"

void _start(void) {
  runtime_init();
  exit_syscall(42);
}
