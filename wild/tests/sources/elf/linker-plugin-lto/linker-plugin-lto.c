//#AbstractConfig:default
// TODO: Investigate why we don't emit _IO_stdin_used which is in .rodata.
//#DiffIgnore:section.rodata
//#DiffIgnore:section.got
//#RequiresLinkerPlugin:true

//#AbstractConfig:error
//#RequiresLinkerPlugin:true

//#Config:gcc:default
//#CompArgs:-flto
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#LinkerDriver:gcc
//#LinkArgs:-flto -nostdlib -znow

//#Config:clang:default
//#Compiler:clang
//#CompArgs:-flto
//#LinkerDriver:clang
//#LinkArgs:-Wl,-znow -flto -nostdlib -O0
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#DiffIgnore:section.eh_frame.type

//#Config:clang-thin:default
//#Compiler:clang
//#CompArgs:-flto=thin
//#LinkerDriver:clang
//#LinkArgs:-Wl,-znow -flto=thin -nostdlib -O0
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#DiffIgnore:section.eh_frame.type

//#Config:clang-link-gcc:error
//#Compiler:clang
//#CompArgs:-flto
//#LinkerDriver:gcc
//#ReferenceLinkers:
//#LinkArgs:-Wl,-znow -flto -nostdlib
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#ExpectError:(contains LLVM-IR, but the linker plugin|Wild was compiled without linker-plugin support)

//#Config:gcc-link-clang:error
//#Compiler:gcc
//#CompArgs:-flto
//#LinkerDriver:clang
//#ReferenceLinkers:
//#LinkArgs:-Wl,-znow -flto -nostdlib
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#ExpectError:(contains GCC-IR, but the linker plugin|Wild was compiled without linker-plugin support)
//#Cross:false

// LTO, no --plugin from the driver. Wild auto-discovers LLVMgold.so.
//#Config:clang-no-plugin:default
//#Compiler:clang
//#CompArgs:-flto
//#LinkerDriver:clang
//#ReferenceLinkers:
//#LinkArgs:-Wl,-znow -nostdlib
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#DiffIgnore:section.eh_frame.type
//#DiffEnabled:false

// Direct wild invocation, no --plugin. Auto-discovers liblto_plugin.so.
//#Config:gcc-no-plugin:default
//#Compiler:gcc
//#CompArgs:-flto
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#LinkArgs:-nostdlib -znow
//#ReferenceLinkers:
//#DiffEnabled:false

//#Config:gcc-thin:default
//#Compiler:gcc
//#CompArgs:-flto=auto
//#LinkerDriver:gcc
//#LinkArgs:-flto=auto -nostdlib -znow
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c

// The only LTO input is in an archive and we end up not using it.
//#Config:clang-empty-lto:default
//#Compiler:clang
//#Object:runtime.c
//#Archive:empty.c:-flto
//#Object:linker-plugin-lto-2.c
//#ReferenceLinkers:
//#LinkerDriver:clang
//#LinkArgs:-flto -nostdlib
//#DiffEnabled:false

// The only LTO input is in an archive and we end up not using it.
//#Config:gcc-empty-lto:default
//#Compiler:gcc
//#Object:runtime.c
//#Archive:empty.c:-flto
//#Object:linker-plugin-lto-2.c
//#LinkerDriver:gcc
//#LinkArgs:-flto -nostdlib -znow

//#Config:gcc-driver-with-unclaimed-llvm-ir-archive:default
//#Compiler:clang
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#Archive:unclaimed.c:-flto
//#ReferenceLinkers:
//#LinkerDriver:gcc
//#LinkArgs:-flto -nostdlib

// Linker message with format string.
//#Config:clang-format-string:error
//#Compiler:clang
//#LinkerDriver:clang
//#ReferenceLinkers:
//#LinkArgs:-Wl,-znow -flto -nostdlib -Wl,-plugin-opt=jobs=foo
//#Archive:empty.c:-flto
//#ExpectError:Error from linker plugin: Invalid parallelism level: foo

//#Config:plugin-not-found:error
//#Compiler:clang
//#CompArgs:-flto
//#LinkArgs:--plugin=/does/not/exist
//#ExpectError:No such file or directory

//#Config:gcc-fat-objects:default
//#Compiler:gcc
//#CompArgs:-flto -ffat-lto-objects -O1
//#LinkerDriver:gcc
//#LinkArgs:-Wl,-znow -flto -ffat-lto-objects -nostdlib
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#DiffIgnore:section-diff-failed..text
//#DoesNotContain: foo

//#Config:clang-fat-objects:default
//#Compiler:clang
//#CompArgs:-flto -ffat-lto-objects -O1
//#LinkerDriver:clang
//#LinkArgs:-Wl,-znow -flto -ffat-lto-objects -nostdlib
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#DiffIgnore:section-diff-failed..text
//#DoesNotContain: foo

// Fat LTO objects linked without -flto on the link line (native ELF, or IR if claimed).
//#Config:gcc-fat-native:default
//#Compiler:gcc
//#CompArgs:-flto -ffat-lto-objects -O1
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#LinkArgs:-nostdlib -znow
//#ReferenceLinkers:
//#DiffEnabled:false

//#Config:clang-fat-native:default
//#Compiler:clang
//#CompArgs:-flto -ffat-lto-objects -O1
//#Object:runtime.c
//#Object:linker-plugin-lto-2.c
//#LinkArgs:-nostdlib -znow
//#ReferenceLinkers:
//#DiffEnabled:false

#include "../common/runtime.h"

int foo();

void _start(void) {
  runtime_init();
  exit_syscall(foo());
}
