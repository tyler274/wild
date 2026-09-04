//#AbstractConfig:default
//#Mode:dynamic
//#CompArgs:-fPIC
//#LinkerScript:linker-script-data-segment.ld
//#LinkArgs:-shared -z now
//#RunEnabled:false
//#DiffEnabled:true
//#ReferenceLinkers:bfd
//#SkipArch:riscv64
// GNU ld and Wild disagree on some PHDR bookkeeping when the script omits PHDRS.
//#DiffIgnore:segment.LOAD.RWX.alignment
//#DiffIgnore:segment.LOAD.RX.alignment
//#DiffIgnore:segment.LOAD.RW.alignment
//#DiffIgnore:section.text.alignment

//#Config:shared:default

int data_item = 42;

int foo(void) {
  return data_item;
}
