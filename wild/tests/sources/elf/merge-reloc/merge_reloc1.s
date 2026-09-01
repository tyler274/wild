.section .altinstructions, "aM", @progbits, 8
.align 8

.globl rec1
rec1:
    .reloc rec1, R_X86_64_64, t1
    .quad 0

.section .data, "aw", @progbits
.globl t1
t1:
    .byte 1
