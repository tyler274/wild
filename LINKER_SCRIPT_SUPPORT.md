# Linker Script Support

This page documents which linker script features Wild supports, which are partially implemented,
and which are planned for the future. Each feature is marked with one of four statuses: `✅`
(supported), `🧪` (partial), `📅` (planned), or `❌` (not planned). A dedicated section at the
end lists the features required to link the Linux kernel.

GNU ld is the kernel and linker-script oracle. LLD and Mold are general-ELF oracles via the
external test suites. Intentional Wild-specific behaviour is documented rather than blindly
matching all three.

## Top-Level Commands

| Feature | Status | Notes |
|---------|--------|-------|
| `GROUP(files...)` | ✅ | |
| `INPUT(files...)` | ✅ | |
| `AS_NEEDED(files...)` | ✅ | |
| `INCLUDE(file)` | ✅ | Recursively expanded with cycle detection; searched relative to the including script, then `-L` / sysroot |
| `OUTPUT_FORMAT(...)` | 🧪 | Emits error if the format does not match the target |
| `OUTPUT_ARCH(arch)` | 🧪 | Parsed and ignored |
| `OUTPUT(filename)` | ❌ | |
| `SECTIONS { ... }` | ✅ | |
| `ENTRY(symbol)` | ✅ | |
| `VERSION { ... }` | ✅ | |
| `PROVIDE(sym = expr)` | ✅ | |
| `PROVIDE_HIDDEN(sym = expr)` | ✅ | |
| `ASSERT(expr, "msg")` | ✅ | |
| `MEMORY { ... }` | ✅ | Regions, `(rwx)` flags, `>region`, and `AT>region` |
| `REGION_ALIAS(alias, region)` | ❌ | |
| `SEARCH_DIR(path)` | ❌ | |
| `STARTUP(filename)` | ❌ | |
| `TARGET(bfdname)` | ❌ | |
| `NOCROSSREFS(sections...)` | ❌ | |
| `INSERT [AFTER\|BEFORE] section` | ❌ | |
| Top-level symbol assignment (`sym = expr`) | ✅ | Constant assignments are available during layout |
| Compound assignment operators (`+=`, `-=`, etc.) | ✅ | |
| `PHDRS` command for explicit program header definition | ✅ | `FILEHDR`, `PHDRS`, `FLAGS`, and `AT(expr)` |

## SECTIONS Block

| Feature | Status | Notes |
|---------|--------|-------|
| Output section definitions (`name : { ... }`) | ✅ | |
| Input section matchers (`*(pattern)`, `file(pattern)`) | ✅ | |
| Glob patterns in section and file names | ✅ | |
| `KEEP(...)` to prevent garbage collection | ✅ | |
| `PROVIDE(sym = expr)` inside sections | ✅ | |
| `PROVIDE_HIDDEN(sym = expr)` inside sections | ✅ | |
| Symbol assignment inside sections (`sym = .`) | ✅ | |
| Location counter assignment (`. = expr`) | 🧪 | Constant expressions and script-defined constants (e.g. `LOAD_OFFSET`) are supported. `. = object_symbol` (as in x86 `srso_alias_untrain_ret`) is not yet supported |
| `ALIGN(n)` on the location counter (`. = ALIGN(n)`) | ✅ | |
| Per-section `ALIGN(n)` specifier | ✅ | |
| `ASSERT(expr, "msg")` inside `SECTIONS` | ✅ | |
| `OVERLAY { ... }` | ✅ | Shared VMA, consecutive LMAs, `__load_start_*` / `__load_stop_*` |
| Output section type specifiers (`(NOLOAD)`, `(COPY)`, etc.) | 🧪 | Setting the section type using the `TYPE` attribute is not yet supported |
| `FILL(value)` | ✅ | Sets the fill pattern for subsequent gaps in the output section |
| `=fillexp` | ✅ | |
| `AT(addr)` load-address specifier on output sections | ✅ | |
| Numeric address between section name and `:` (e.g. `name 0 : { ... }`) | 🧪 | Only numeric literals are currently supported |
| `SORT(...)`, `SORT_BY_NAME(...)` | ✅ | |
| `SORT_BY_ALIGNMENT(...)` | ✅ | Parsed and ignored, as sections are sorted by alignment by default |
| `SORT_BY_INIT_PRIORITY(...)` | ✅ | Uses GCC `init_priority` encoded in `.init_array.N` / `.ctors.N` names |
| `EXCLUDE_FILE(...)` inside input section matchers | ✅ | Both `*(EXCLUDE_FILE(a.o) .text)` and `EXCLUDE_FILE(a.o) *(.text)` |
| `BYTE(expr)`, `SHORT(expr)`, `LONG(expr)`, `QUAD(expr)` output data | ✅ | Written in the target endianness |
| `SUBALIGN(n)` forced input alignment | ❌ | |
| `ONLY_IF_RO` / `ONLY_IF_RW` output section constraints | ❌ | |
| `:phdr` output section phdrs | ✅ | |

## Expressions and Functions

| Feature | Status | Notes |
|---------|--------|-------|
| Arithmetic operators: `+`, `-`, `*`, `/` | ✅ | |
| Comparison operators: `<`, `>`, `<=`, `>=`, `==`, `!=` | ✅ | |
| Bitwise operators: `&`, `\|`, `^`, `~`, `<<`, `>>` | ✅ | |
| Logical operators: `&&`, `\|\|` | ✅ | |
| Unary operators: `-`, `!`, `~` | ✅ | |
| Numeric literals: decimal and hexadecimal | ✅ | |
| Numeric literal K/M suffixes (e.g. `64K`, `2M`) | ✅ | |
| Symbol references and location counter (`.`) | ✅ | Constant script symbols are resolved during layout |
| Parenthesised sub-expressions | ✅ | |
| `SIZEOF(section)` | ✅ | |
| `ALIGNOF(section)` | ✅ | |
| `ADDR(section)` | ✅ | |
| `LOADADDR(section)` | ✅ | Returns the section LMA |
| `ALIGN(expr)` | ✅ | |
| `LENGTH(region)` | ✅ | |
| `ORIGIN(region)` | ✅ | |
| `MIN(a, b)` | ✅ | |
| `MAX(a, b)` | ✅ | |
| Ternary operator (`condition ? a : b`) | ✅ | |
| `DEFINED(sym)` | ✅ | |
| `SIZEOF_HEADERS` | ✅ | |
| `SEGMENT_START(segment, default)` | ✅ | Supports `"text"`, `"data"`, `"bss"`, `"rodata"`; returns `-Ttext`/`-Tdata`/`-Tbss` override if provided, otherwise `default`; unknown segment names always return `default` |

## MEMORY Command

The `MEMORY` command defines named memory regions with an origin address and a length. Wild parses
`MEMORY` blocks including the `ORIGIN`/`org`/`o` and `LENGTH`/`len`/`l` attribute keywords,
`(rwx)` attribute flags, `>region` VMA placement, and `AT>region` load-region placement. If a
section has no explicit `>region`, a compatible region is selected from the flags as GNU ld does.

| Feature | Status | Notes |
|---------|--------|-------|
| `MEMORY { ... }` block parsing | ✅ | |
| Region name | ✅ | |
| `ORIGIN`/`org`/`o` attribute | ✅ | |
| `LENGTH`/`len`/`l` attribute | ✅ | |
| Attribute flags (`(rwx)`, `(rx)`, etc.) | ✅ | Used to auto-pick a region when `>region` is omitted |
| `>region` output section placement | ✅ | |
| `AT>region` load-region placement | ✅ | Distinct per-region LMA cursor |

## Linux Kernel Requirements

The Linux kernel's build system uses a rich set of linker script features across `vmlinux.lds` and
related architecture-specific scripts. The table below lists each such feature along with its
current status. Kernel-like scripts for x86_64, aarch64, riscv64, loongarch64, and ppc64le are
covered by Wild's integration tests; full `vmlinux` / module links should be validated against GNU
ld with `readelf` and, where possible, QEMU boot.

| Feature | Status | Notes |
|---------|--------|-------|
| `OVERLAY { ... }` sections | ✅ | ARM vectors and similar overlays |
| Output section type specifiers (`(NOLOAD)`, `(COPY)`) | ✅ | The `TYPE` attribute is not used in the kernel |
| `FILL(value)` | ✅ | |
| `=fillexp` | ✅ | |
| `AT(addr)` load-address specifier on output sections | ✅ | |
| `>region` memory region placement | ✅ | |
| `AT>region` load-region placement | ✅ | |
| `SORT(...)`, `SORT_BY_NAME(...)` | ✅ | |
| `SORT_BY_ALIGNMENT(...)` | ✅ | Parsed and ignored, as sections are sorted by alignment by default |
| `SORT_BY_INIT_PRIORITY(...)` | ✅ | |
| `EXCLUDE_FILE(...)` inside input section matchers | ✅ | |
| `BYTE` / `SHORT` / `LONG` / `QUAD` | ✅ | Used by RISC-V/EFI kernel scripts |
| `INCLUDE(file)` | ✅ | Module/arch fragments; the kernel itself is cpp-preprocessed |
| `CONSTRUCTORS` command | ✅ | Parsed and ignored, it is a nop for ELF |
| `PHDRS` command for explicit program header definition | ✅ | Including `AT(expr)` |
| Ternary operator (`condition ? a : b`) | ✅ | |
| `DEFINED(sym)` function | ✅ | |
| `SIZEOF_HEADERS` built-in symbol | ✅ | |
| `/DISCARD/` command | ✅ | |
