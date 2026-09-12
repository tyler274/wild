__attribute__((used, section(".text.rev.bbb"))) int func_bbb(void) { return 2; }

int prio_200 __attribute__((section(".init_array.200"))) = 2;
