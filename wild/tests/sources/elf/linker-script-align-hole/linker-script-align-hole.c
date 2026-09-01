//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-align-hole.ld
//#LinkArgs:--no-gc-sections
//#ExpectSym:after_var section=".after"
//#ExpectSym:__init_end address=0x401000
//#NoSym:__ehdr_start
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default

char before_bytes[0x428] __attribute__((section(".before")));

__attribute__((section(".after"))) int after_var = 1;

void _start(void) {
    (void)before_bytes;
    (void)after_var;
}
