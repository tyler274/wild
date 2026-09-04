// Plugin codegen must be placed at the first IR input's command-line position
// (GNU ld), not after every non-LTO object. Functions in .init from an ELF file
// before the IR, the LTO object, and an ELF file after the IR must appear in
// that order.

//#AbstractConfig:default
//#RequiresLinkerPlugin:true
//#Arch: x86_64
//#DiffEnabled:false

//#Config:gcc:default
//#LinkerDriver:gcc
//#Object:lto-init-order-ir.c:-flto
//#Object:lto-init-order-after.c
//#Object:runtime.c
//#LinkArgs:-flto -nostdlib -znow

//#Config:clang:default
//#Compiler:clang
//#LinkerDriver:clang
//#Object:lto-init-order-ir.c:-flto
//#Object:lto-init-order-after.c
//#Object:runtime.c
//#LinkArgs:-Wl,-znow -flto -nostdlib

//#Config:clang-thin:default
//#Compiler:clang
//#LinkerDriver:clang
//#Object:lto-init-order-ir.c:-flto=thin
//#Object:lto-init-order-after.c
//#Object:runtime.c
//#LinkArgs:-Wl,-znow -flto=thin -nostdlib

//#Config:clang-fat:default
//#Compiler:clang
//#LinkerDriver:clang
//#Object:lto-init-order-ir.c:-flto -ffat-lto-objects
//#Object:lto-init-order-after.c
//#Object:runtime.c
//#LinkArgs:-Wl,-znow -flto -ffat-lto-objects -nostdlib

#include "../common/runtime.h"

void init_before(void) __attribute__((section(".init"), used));
void init_before(void) {}

extern void init_ir(void);
extern void init_after(void);

void _start(void) {
  runtime_init();
  if (!((char *)init_before < (char *)init_ir &&
        (char *)init_ir < (char *)init_after)) {
    exit_syscall(101);
  }
  exit_syscall(42);
}
