//#AbstractConfig:base
//#Object:runtime.c
//#CompArgs:-fno-asynchronous-unwind-tables -fno-ident -g0
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le

//#Config:place:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=place
//#ExpectSection:.orphan_data
//#ExpectSym:orphan_var section=".orphan_data"

//#Config:warn:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=warn
//#ExpectSection:.orphan_data
//#ExpectWarning:orphan section

//#Config:discard:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=discard
//#NoSection:.orphan_data
//#NoSym:orphan_var

//#Config:error:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=error
//#ExpectError:orphan section

#include "../common/runtime.h"

int orphan_var __attribute__((used, section(".orphan_data"))) = 42;

void _start(void) {
    runtime_init();
    (void)orphan_var;
    exit_syscall(42);
}
