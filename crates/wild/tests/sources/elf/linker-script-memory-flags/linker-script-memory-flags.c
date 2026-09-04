//#AbstractConfig:default
//#Config:basic:default
//#RunEnabled:false
//#LinkArgs:-shared
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-memory-flags.ld

static int data_var __attribute__((used, section(".data"))) = 1;

void _start(void) {}
