// GNU `LD_FEATURE("SANE_EXPR")`: treat numbers as numbers everywhere.
// Without parsing the command, GNU ld (and Wild) would take it as an input file.

//#AbstractConfig:default
//#Arch: x86_64
//#Object:runtime.c
//#RunEnabled:false
//#LinkArgs:-nostdlib -znow --no-gc-sections
//#ReferenceLinkers:bfd
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:sane:default
//#LinkerScript:script-ld-feature.ld
//#ExpectSym:e3 address=0x8000,section="ABS"

//#Config:unknown:default
//#LinkerScript:script-ld-feature-unknown.ld
//#ExpectError:unknown feature

#include "../common/runtime.h"

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
