//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#SkipOverlapSegmentsCheck:true
//#DiffIgnore:section.got
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:basic:default
//#LinkerScript:linker-script-align-vma.ld
//#ExpectSym:_stext address=0x400004
//#ExpectSym:aligned_sym address=0x400010
//#ExpectProgramHeader:LOAD flags=RX,vaddr=0x400004

//#Config:two-load:default
//#LinkerScript:linker-script-align-vma-two-load.ld
//#WildExtraLinkArgs:-z max-page-size=0x200000
//#ExpectProgramHeader:LOAD flags=RX,vaddr=0x400000
//#ExpectProgramHeader:LOAD flags=RW,vaddr=0x600000

void _start(void) {}
int payload = 1;
