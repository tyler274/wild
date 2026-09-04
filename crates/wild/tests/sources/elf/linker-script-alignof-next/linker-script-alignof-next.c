//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkerScript:linker-script-alignof-next.ld
//#LinkArgs:--no-gc-sections
//#ExpectSym:__bss_start address=0x401020,alignment=32
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default

char data_byte = 1;

__attribute__((aligned(32))) char bss_item[8];

void _start(void) {
    (void)data_byte;
    (void)bss_item;
}
