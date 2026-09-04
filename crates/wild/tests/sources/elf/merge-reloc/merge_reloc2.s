.section .altinstructions, "aM", @progbits, 8
.align 8

.globl rec2
rec2:
    .reloc rec2, R_X86_64_64, t2
    .quad 0

.section .data, "aw", @progbits
.globl t2
t2:
    .byte 2
