// rustc `--emit=obj` so the object path stays stable across `--cfg wild_inc`.
// Freestanding x86_64 `_start` so CompArgs are not inherited by C objects.

//#AbstractConfig:base
//#Arch: x86_64
//#WildExtraLinkArgs:--incremental -nostdlib
//#TestIncremental:true
//#DiffEnabled:false
//#CompArgs:--edition=2021 --emit=obj -C panic=abort -C opt-level=0

//#Config:opt0:base

//#Config:opt1:base
//#CompArgs:--edition=2021 --emit=obj -C panic=abort -C opt-level=1

//#Config:opt2:base
//#CompArgs:--edition=2021 --emit=obj -C panic=abort -C opt-level=2

//#Config:opt3:base
//#CompArgs:--edition=2021 --emit=obj -C panic=abort -C opt-level=3

//#Config:opts:base
//#CompArgs:--edition=2021 --emit=obj -C panic=abort -C opt-level=s

#![no_std]
#![no_main]

#[cfg(wild_inc)]
const CODE: i32 = 43;
#[cfg(not(wild_inc))]
const CODE: i32 = 42;

#[no_mangle]
#[used]
pub static wild_inc_marker: i32 = CODE;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // RIP-relative load so every opt-level keeps a reloc, without core panicking
    // checks that `--emit=obj` does not link.
    let marker: i32;
    unsafe {
        core::arch::asm!(
            "mov {sym}(%rip), {val:e}",
            sym = sym wild_inc_marker,
            val = out(reg) marker,
            options(att_syntax, nostack, preserves_flags, readonly)
        );
    }
    let code = if marker != CODE { 101 } else { marker };
    unsafe {
        core::arch::asm!(
            "mov $60, %eax",
            "syscall",
            "ud2",
            in("rdi") code,
            options(noreturn, att_syntax)
        );
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
