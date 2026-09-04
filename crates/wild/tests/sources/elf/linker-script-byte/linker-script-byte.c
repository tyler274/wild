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

const char keep[] __attribute__((used, section(".rodata"))) = "keep";

int main() { return 0; }
