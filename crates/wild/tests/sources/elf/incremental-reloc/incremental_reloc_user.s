// Direct R_X86_64_PC32 to `target`. GOT-relative loads would be fixed by rewriting
// the GOT and would not exercise reverse-reloc patching of skipped objects.

.section .text,"ax",@progbits

.globl _start
.type _start, @function
_start:
    endbr64
    movl    target(%rip), %eax
    cmpl    $42, %eax
    jne     fail
    mov     $42, %rdi
    call    exit_syscall
fail:
    mov     $101, %rdi
    call    exit_syscall
.size _start, .-_start
