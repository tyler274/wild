# GNU `FORCE_GROUP_ALLOCATION` / `--force-group-allocation`: resolve ELF
# section groups even for `-r`. Wild always does this; GNU ld needs the
# command or flag. Members become normal sections and `.group` is dropped.

//#AbstractConfig:default
//#Arch: x86_64
//#RunEnabled:false
//#LinkArgs:-r
//#ReferenceLinkers:bfd
//#ExpectSection:.text.group_member flags=AX
//#NoSection:.group

//#Config:script:default
//#LinkerScript:script-force-group.ld

//#Config:cli:default
//#LinkArgs:-r --force-group-allocation

.section .text.group_member,"axG",@progbits,group_signature,comdat
.globl group_signature
.type group_signature,@function
group_signature:
  ret
.size group_signature, .-group_signature
