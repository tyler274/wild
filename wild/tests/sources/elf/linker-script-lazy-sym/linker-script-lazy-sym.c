//#AbstractConfig:default
//#Config:basic:default
//#RunEnabled:false
//#LinkArgs:-T tests/sources/elf/linker-script-lazy-sym/linker-script-lazy-sym.ld
//#ReferenceLinkers:bfd
//#ExpectSym:__global_pointer$

void _start(void) {}
