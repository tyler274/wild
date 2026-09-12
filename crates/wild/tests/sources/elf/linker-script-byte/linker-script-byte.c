//#AbstractConfig:default
//#Config:basic:default
//#Mode:dynamic
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:-shared -z now
//#LinkerScript:linker-script-byte.ld
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment
//#ExpectSectionBytes:.rodata=0x6b65657000 0..5
//#ExpectSectionBytes:.rodata=0x00 5..6
//#ExpectSectionBytes:.rodata=0x3322 6..8
//#ExpectSectionBytes:.rodata=0x77665544 8..12
//#ExpectSectionBytes:.rodata=0xffeeddccbbaa9988 12..20

const char keep[] __attribute__((used, section(".rodata"))) = "keep";

int main() { return 0; }
