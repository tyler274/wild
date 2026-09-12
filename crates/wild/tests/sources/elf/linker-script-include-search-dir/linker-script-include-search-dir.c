// INCLUDE searches SEARCH_DIR the same way GNU ld searches -L.
// fragment.ld is not next to this script, so the script-directory lookup fails.
//
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#DiffEnabled:false
//#LinkArgs:--no-gc-sections
//#Object:runtime.c
//#AugmentLinkerScript:linker-script-include-search-dir.ld
//#ExpectSym:from_include address=0x1234

#include "../common/runtime.h"

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
