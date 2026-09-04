//#AbstractConfig:default
//#Config:basic:default
//#Mode:dynamic
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:-shared -z now
//#LinkerScript:linker-script-fill-cmd.ld
//#ExpectSectionBytes:.fill1=0x119090909090909022 0..9
//#DiffIgnore:section.got
//#DiffIgnore:section.fill1
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

__attribute__((section(".fill1.first"), aligned(8))) char fill1_first = 0x11;
__attribute__((section(".fill1.second"), aligned(8))) char fill1_second = 0x22;

int main() { return 0; }
