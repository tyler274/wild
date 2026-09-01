//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#SkipOverlapSegmentsCheck:true
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default
//#LinkerScript:linker-script-align-vma.ld
//#ExpectSym:_stext address=0x400004,section=".text"
//#ExpectSym:aligned_sym address=0x400010
//#ExpectSym:_etext section=".text"
//#ExpectSym:__init_begin section=".text"
//#ExpectProgramHeader:LOAD flags=RX,vaddr=0x400004

//#Config:two-load:default
//#LinkerScript:linker-script-align-vma-two-load.ld
//#WildExtraLinkArgs:-z max-page-size=0x200000
//#ExpectProgramHeader:LOAD flags=RX,vaddr=0x400000
//#ExpectProgramHeader:LOAD flags=RW,vaddr=0x600000

//#Config:end-sym:default
//#LinkerScript:linker-script-align-vma-end.ld
//#ExpectSym:_end section=".bss"

//#Config:exit-align:default
//#LinkerScript:linker-script-input-align.ld

//#Config:orc-flags:default
//#LinkerScript:linker-script-orc-flags.ld
//#ExpectSection:.orc_lookup flags=WA,type=0x8

void _start(void) {}
int payload = 1;
char bss_byte __attribute__((used));

__attribute__((section(".exit.text"), aligned(16), used))
static const char exit_text_bytes[16] = {1};
