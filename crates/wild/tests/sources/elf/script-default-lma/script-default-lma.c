// GNU default LMA without `AT` / `AT>`: keep the previous allocatable
// section's VMA−LMA difference when a compatible MEMORY region already
// contains a section. A specific VMA (`.data 0x18000 :`) sets LMA = VMA.

//#AbstractConfig:default
//#Arch: x86_64
//#RunEnabled:false
//#LinkArgs:-shared
//#ReferenceLinkers:bfd
//#Object:script-default-lma.s
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RW.alignment
//#DiffIgnore:segment.LOAD.RWX.alignment

//#Config:memory:default
//#LinkerScript:script-default-lma.ld

//#Config:no-memory:default
//#LinkerScript:script-default-lma-at.ld

//#Config:specific-vma:default
//#LinkerScript:script-default-lma-specific.ld

void _start(void) {}
