//#AbstractConfig:default
//#RunEnabled:false
//#LinkArgs:-shared
//#ReferenceLinkers:bfd

//#Config:at-region:default
//#LinkerScript:linker-script-at-region.ld

static int data_var __attribute__((used, section(".data"))) = 1;

void _start(void) {}
