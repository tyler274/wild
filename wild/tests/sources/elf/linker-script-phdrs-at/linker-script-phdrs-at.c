//#AbstractConfig:default
//#Config:basic:default
//#RunEnabled:false
//#LinkArgs:-T tests/sources/elf/linker-script-phdrs-at/linker-script-phdrs-at.ld
//#ReferenceLinkers:bfd
//#ExpectProgramHeader:LOAD paddr=0x1000

void _start(void) {}
int data_var = 1;
