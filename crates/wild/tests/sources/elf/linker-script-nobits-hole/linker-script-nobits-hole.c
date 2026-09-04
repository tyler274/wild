//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-nobits-hole.ld
//#LinkArgs:--no-gc-sections
//#ExpectSym:after_bss_var section=".after_bss"
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default

char bss_space[0x1000];

__attribute__((section(".after_bss"))) int after_bss_var = 1;

void _start(void) {
    (void)bss_space;
    (void)after_bss_var;
}
