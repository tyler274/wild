// GNU `EXTERN(symbol)` is `-u`: pull the archive member that defines `bar`.
//
//#Config:extern-archive
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#DiffEnabled:false
//#Object:runtime.c
//#LinkerScript:linker-script-extern.ld
//#Archive:archive_activation0.c
//#ExpectSym:bar
//#ExpectSym:is_archive0_loaded

#include "../common/runtime.h"

__attribute__((weak)) int is_archive0_loaded() { return 0; }

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
