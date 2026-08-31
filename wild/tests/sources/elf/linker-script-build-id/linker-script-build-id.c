//#AbstractConfig:default
//#RunEnabled:false
//#ReferenceLinkers:bfd
//#LinkArgs:--build-id=0xdeadbeef
//#DiffIgnore:section.got
//#DiffIgnore:section.notes
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:notes:default
//#LinkerScript:linker-script-build-id.ld
//#ExpectSection:.notes type=7
//#NoSection:.note.gnu.build-id

//#Config:discard:default
//#LinkerScript:linker-script-build-id-discard.ld
//#NoSection:.note.gnu.build-id
//#NoSection:.notes

void _start(void) {}
