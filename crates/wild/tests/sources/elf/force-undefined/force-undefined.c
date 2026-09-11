//#AbstractConfig:default
//#Object:runtime.c

//#Config:undefined:default
//#LinkArgs:--undefined=foo -u bar -ubaz
//#ExpectSym:foo
//#ExpectSym:bar
//#ExpectSym:baz
//#TestUpdateInPlace:true

// Verify that we can activate an archive entry by listing a symbol it defines as undefined.
//#Config:archive-activation:default
//#Archive:archive_activation0.c
//#CompArgs:-DEXPECT_ARCH0
//#LinkArgs:--undefined=bar
//#ExpectSym:bar
//#ExpectSym:is_archive0_loaded

// `--require-defined` is `-u` plus an error if the symbol is never defined.
// The archive member is kept by `--require-defined=bar` alone (no other refs).
//#Config:require-defined:default
//#Archive:archive_activation0.c
//#LinkArgs:--require-defined=bar
//#ExpectSym:bar
//#ExpectSym:is_archive0_loaded

//#Config:require-defined-missing:default
//#LinkArgs:--require-defined=xyz
//#ExpectError:xyz

#include "../common/runtime.h"

__attribute__((weak)) int is_archive0_loaded() { return 0; }

void _start(void) {
  runtime_init();

#ifdef EXPECT_ARCH0
  if (!is_archive0_loaded()) {
    exit_syscall(10);
  }
#endif

  exit_syscall(42);
}
