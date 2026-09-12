// GNU `FORCE_COMMON_ALLOCATION` / `-d`: allocate commons even for `-r`.
// Without it, a relocatable link leaves commons as `SHN_COMMON`.
//
//#AbstractConfig:default
//#CompArgs:-fcommon
//#RunEnabled:false
//#LinkArgs:-r
//#ReferenceLinkers:bfd

//#Config:keep-common:default
//#ExpectSym:common_var section="COMMON",size=4

//#Config:force-script:default
//#AugmentLinkerScript:script-force-common.ld
//#ExpectSym:common_var section=".bss",size=4

//#Config:force-cli:default
//#LinkArgs:-r -d
//#ExpectSym:common_var section=".bss",size=4

int common_var;
