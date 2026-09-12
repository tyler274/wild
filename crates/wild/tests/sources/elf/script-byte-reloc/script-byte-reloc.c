// Relocatable `-r` has no PT_LOAD, so BYTE/SHORT after input contents must
// occupy file space without overlapping the next output section.

//#Config:default
//#RunEnabled:false
//#LinkArgs:-r
//#LinkerScript:script-byte-reloc.ld
//#ReferenceLinkers:bfd
//#ExpectSectionBytes:.data=0xaa000000 0..4
//#ExpectSectionBytes:.data=0x11 4..5
//#ExpectSectionBytes:.data=0x3322 5..7

int data_var __attribute__((used, section(".data"))) = 0xaa;
const char keep[] __attribute__((used, section(".rodata"))) = "keep";

int mod_fn(void) { return 1; }
