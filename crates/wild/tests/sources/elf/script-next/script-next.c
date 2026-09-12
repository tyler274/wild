// GNU `NEXT(exp)` is `ALIGN(exp)` when MEMORY does not define discontinuous
// regions. Two-arg `ALIGN(exp, align)` aligns an arbitrary value.

//#Config:default
//#Object:runtime.c
//#RunEnabled:false
//#LinkArgs:-nostdlib -znow --no-gc-sections
//#LinkerScript:script-next.ld
//#ReferenceLinkers:bfd
//#SkipArch:riscv64,ppc64le
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment
//#ExpectSym:next_at address=0x2000
//#ExpectSym:align_at address=0x2000
//#ExpectSym:two_arg address=0x2000

#include "../common/runtime.h"

void _start(void) {
    runtime_init();
    exit_syscall(42);
}
