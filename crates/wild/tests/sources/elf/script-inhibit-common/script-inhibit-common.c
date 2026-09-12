// GNU `INHIBIT_COMMON_ALLOCATION` / `--no-define-common` is for shared
// objects: commons become undefined instead of `.bss`. GNU ld rejects the
// CLI flag without `-shared`.

//#AbstractConfig:default
//#CompArgs:-fcommon -fPIC
//#RunEnabled:false
//#LinkArgs:-shared -z now --no-gc-sections
//#ReferenceLinkers:bfd
//#DiffIgnore:.dynamic.DT_RELA
//#DiffIgnore:.dynamic.DT_RELAENT

//#Config:allocate:default
//#ExpectSym:common_var section=".bss",size=4

//#Config:inhibit-script:default
//#LinkerScript:script-inhibit-common.ld

//#Config:inhibit-cli:default
//#LinkArgs:-shared -z now --no-gc-sections --no-define-common

//#Config:cli-needs-shared:default
//#LinkArgs:--no-define-common
//#ExpectError:without -shared

int common_var;
int *keep_common = &common_var;

void _start(void) {}
