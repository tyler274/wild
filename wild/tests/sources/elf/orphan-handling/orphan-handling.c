//#AbstractConfig:base
//#Object:runtime.c
//#CompArgs:-fno-asynchronous-unwind-tables -fno-ident -g0
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld
//#ReferenceLinkers:bfd
//#RunEnabled:false
//#SkipArch:riscv64,ppc64le

//#Config:place:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=place
//#ExpectSection:.orphan_text after=".text"
//#ExpectSection:.orphan_data after=".data"
//#ExpectSym:orphan_var section=".orphan_data"
//#ExpectSym:orphan_fn section=".orphan_text"

//#Config:warn:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=warn
//#ExpectSection:.orphan_data
//#ExpectWarning:orphan section

//#Config:discard:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=discard
//#NoSection:.orphan_data
//#NoSection:.orphan_text
//#NoSym:orphan_var

//#Config:error:base
//#LinkArgs:-nostdlib -znow --no-gc-sections -T ./orphan-handling.ld --orphan-handling=error
//#ExpectError:orphan section

#include "../common/runtime.h"

int in_data __attribute__((used)) = 7;
int orphan_var __attribute__((used, section(".orphan_data"))) = 42;

void orphan_fn(void) __attribute__((used, section(".orphan_text")));
void orphan_fn(void) {}

void _start(void) {
    runtime_init();
    (void)in_data;
    (void)orphan_var;
    exit_syscall(42);
}
