// GNU ld does not bind an unversioned reference to a DSO symbol that has only
// a non-default (hidden) version. `-z defs` therefore fails. Glibc's libnsl
// avoids this by linking against `linkobj/libc.so`, where those RPC symbols
// are default versions.

//#Config:default
//#SkipArch: ppc64le
//#Mode:dynamic
//#CompArgs:-fPIC
//#CompSoArgs:-fPIC
//#LinkArgs:--shared -z defs -z now
//#LinkSoArgs:--version-script=./hidden-dso-version-lib.map
//#Shared:hidden-dso-version-lib.c
//#ExpectError:foo

int foo(void);

int bar(void) { return foo(); }
