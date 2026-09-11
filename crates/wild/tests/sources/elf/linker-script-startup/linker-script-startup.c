// GNU STARTUP makes that file the first input of the link, so its `.order`
// contribution precedes this file's. `-T` is required so GNU processes STARTUP
// as part of the main script rather than as an implicit input script.
//
//#Config:startup
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:-L$OUT_DIR --no-gc-sections
//#DiffEnabled:false
//#LinkerScript:linker-script-startup.ld
//#Object:noadd:startup-first.c
//#ExpectSym:startup_marker section=".order",offset-in-section=0
//#ExpectSym:main_marker section=".order",offset-in-section=1

__attribute__((section(".order"), used))
char main_marker = 2;

void _start(void) {}
