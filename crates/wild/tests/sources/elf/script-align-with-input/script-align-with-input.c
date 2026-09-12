// GNU `ALIGN_WITH_INPUT`: keep the VMA−LMA difference when aligning an output
// section to its inputs. Independent LMA alignment would change the delta when
// the load region is not the same alignment as the VMA region.
//
//#Arch: x86_64
//#RunEnabled:false
//#LinkArgs:-shared
//#ReferenceLinkers:bfd
//#LinkerScript:script-align-with-input.ld
//#Object:script-align-with-input.s
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RW.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

void _start(void) {}
