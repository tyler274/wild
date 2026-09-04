.section .rodata.cst8, "aM", @progbits, 8
.align 8

.globl c8a
c8a: .quad 0x1122334455667788

.globl c8unique
c8unique: .quad 0xaabbccdd00000001

.section .rodata.blob, "aM", @progbits, 1
.align 1

.globl blob
blob: .quad 0x1122334455667788
