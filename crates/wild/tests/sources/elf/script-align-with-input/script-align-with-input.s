.section .a, "a", @progbits
.globl a_val
a_val:
.byte 1

.section .b, "a", @progbits
.balign 16
.globl b_val
b_val:
.byte 2
