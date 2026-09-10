// An archive member must not shadow a later regular object's definition of the
// same symbol. Mirrors mold's link-order2.sh (af7d056853): archive a.o defines
// foo=1 and b.o defines bar=3; a later object defines foo=2. Only b.o is
// extracted, so foo()==2 and bar()==3.
//
// The test harness always places the primary object first, so this matches
// mold's `d.o c.o e.a` order.

//#AbstractConfig:default
//#Object:runtime.c
//#Object:foo-object.c
//#Archive:foo-archive.c,bar-archive.c
//#ReferenceLinkers:bfd,lld,mold

//#Config:object-then-archive:default

#include "../common/runtime.h"

int foo(void);
int bar(void);

void _start(void) {
  runtime_init();

  if (foo() != 2) {
    exit_syscall(10);
  }
  if (bar() != 3) {
    exit_syscall(11);
  }
  exit_syscall(42);
}
