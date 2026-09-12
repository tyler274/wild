// Multiple GNU `VERSION` commands combine in command-line order, including
// when they come from separate input scripts (mold linker-script-version.sh).
//
//#AbstractConfig:default
//#Mode:dynamic
//#RunEnabled:false
//#LinkArgs:-shared
//#CompArgs:-fPIC
//#ExpectDynSym:foo
//#ExpectDynSym:bar
//#DiffIgnore:.dynamic.DT_FLAGS*
//#DiffIgnore:.dynamic.DT_RELA
//#DiffIgnore:.dynamic.DT_RELAENT
//#DiffIgnore:file-header.entry
//#DiffIgnore:section.got
//#DiffIgnore:section.rela.dyn

//#Config:two-files:default
//#AugmentLinkerScript:ver-a.ld
//#AugmentLinkerScript:ver-b.ld

//#Config:one-file:default
//#AugmentLinkerScript:ver-both.ld

void foo(void) {}
void bar(void) {}
