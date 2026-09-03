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
| `OUTPUT_FORMAT(...)` | ✅ | Accepted when it matches the link target (`-EL`/`-EB` select the three-arg form). Does not switch architecture; mismatch or unsupported BFD names error |
| `OUTPUT_ARCH(arch)` | ✅ | Accepted when it matches the link target (kernel `i386:x86-64`, `aarch64`, `riscv`, `loongarch`, `powerpc:common64`). Does not switch architecture |
| `OUTPUT(filename)` | ❌ | |
| `SECTIONS { ... }` | ✅ | |
| `ENTRY(symbol)` | ✅ | |
| `VERSION { ... }` | ✅ | |
| `PROVIDE(sym = expr)` | ✅ | Unused PROVIDE is ignored, including when the RHS is undefined |
| `PROVIDE_HIDDEN(sym = expr)` | ✅ | |
| `ASSERT(expr, "msg")` | ✅ | |
| `MEMORY { ... }` | ✅ | Regions, `(rwx)` flags, `>region`, and `AT>region` |
| `REGION_ALIAS(alias, region)` | ❌ | |
| `SEARCH_DIR(path)` | ❌ | |
| `STARTUP(filename)` | ❌ | |
| `TARGET(bfdname)` | ❌ | |
| `NOCROSSREFS(sections...)` | ❌ | |
| `INSERT [AFTER\|BEFORE] section` | ❌ | |
| Top-level symbol assignment (`sym = expr`) | ✅ | Constant assignments are available during layout. `st_shndx` follows GNU ld: a single relocatable residual (symbol or `.`) copies that section; `ABSOLUTE()`, differences of two section symbols, and constants are `SHN_ABS` |
| Compound assignment operators (`+=`, `-=`, etc.) | ✅ | |
| `PHDRS` command for explicit program header definition | ✅ | `FILEHDR`, `PHDRS`, `FLAGS`, and `AT(expr)`. Without `FILEHDR`, ELF headers occupy file space only and do not advance the VMA. A `. = ALIGN(...)` immediately before a new `PT_LOAD` is applied before the LOAD starts, so `p_vaddr` is the script address rather than `max-page-size` plus that address |

## SECTIONS Block

| Feature | Status | Notes |
|---------|--------|-------|
| Output section definitions (`name : { ... }`) | ✅ | Empty sections inherit the location-counter VMA when they sit in a `PT_LOAD`. Sections with an explicit address of 0 (e.g. `.comment 0 :`) stay at 0 and do not contribute to `PT_LOAD` bounds. Empty loadable sections with no file contents are `NOBITS` |
| Input section matchers (`*(pattern)`, `file(pattern)`) | ✅ | `*(.note.*)` absorbs the linker-generated `--build-id` note (GNU ld); `/DISCARD/` of that name drops it. Input `SHT_REL`/`SHT_RELA`/`SHT_SYMTAB`/`SHT_STRTAB` are not matched (`*(.rela.*)` does not fill `.rela.dyn`; `*(.symtab)` does not replace the linker's symbol table). Matchers without `SORT*` keep GNU ld input order and align each input to its own `sh_addralign` |
| Glob patterns in section and file names | ✅ | |
| `KEEP(...)` to prevent garbage collection | ✅ | |
| `PROVIDE(sym = expr)` inside sections | ✅ | |
| `PROVIDE_HIDDEN(sym = expr)` inside sections | ✅ | |
| Symbol assignment inside sections (`sym = .`) | ✅ | Script assignments override prelude section-boundary symbols of the same name in the symbol table (kernel `_etext` is in `.text`, not `SHN_ABS`). An assignment after `. = ALIGN(...)` between sections stays on the previous output section (kernel `_end` in `.brk`). Bare aliases copy the target's `st_shndx` (`jiffies = jiffies_64`); `ABSOLUTE()`, `_etext - _stext`, and `symbol + const` are `SHN_ABS` |
| Location counter assignment (`. = expr`) | 🧪 | Constants, script-defined constants, script assignments (`_etext = .`), and object symbols in already-laid-out sections are supported. Script assignments override prelude section-boundary symbols of the same name. Object symbols are GNU ld absolute addresses, so `. = symbol \| mask` (x86 `srso_alias_untrain_ret`) applies the mask to the VMA. The object-symbol address is the start of that output section/secondary plus the symbol's input offset. Forward references are not supported |
| `ALIGN(n)` on the location counter (`. = ALIGN(n)`) | ✅ | Aligns the absolute VMA, matching GNU ld |
| Per-section `ALIGN(n)` specifier | ✅ | |
| `ASSERT(expr, "msg")` inside `SECTIONS` | ✅ | |
| `OVERLAY { ... }` | ✅ | Shared VMA, consecutive LMAs, `__load_start_*` / `__load_stop_*` |
| Output section type specifiers (`(NOLOAD)`, `(COPY)`, etc.) | ✅ | `(TYPE = SHT_* \| integer)` and `(READONLY (TYPE = ...))`. `TYPE=` is the default `sh_type` when the section has no input-driven type (GNU ld: `BYTE`/`LONG`/etc.); input sections replace it |
| `FILL(value)` | ✅ | Sets the fill pattern for subsequent gaps in the output section |
| `=fillexp` | ✅ | |
| `AT(addr)` load-address specifier on output sections | ✅ | |
| Numeric address between section name and `:` (e.g. `name 0 : { ... }`) | ✅ | Expressions including `ALIGN(n)`, `ADDR`/`SIZEOF`/`LOADADDR`, and `.` (current VMA). `ALIGN(0)` is a no-op, matching GNU ld (powerpc `.text ALIGN(0) :`) |
| `SORT(...)`, `SORT_BY_NAME(...)` | ✅ | |
| `SORT_BY_ALIGNMENT(...)` | ✅ | Descending `sh_addralign`, then name |
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
| Symbol references and location counter (`.`) | ✅ | Constant script symbols and object symbols in already-laid-out sections are resolved during layout |
| Parenthesised sub-expressions | ✅ | |
| `SIZEOF(section)` | ✅ | |
| `ALIGNOF(section)` | ✅ | |
| `ADDR(section)` | ✅ | |
| `LOADADDR(section)` | ✅ | Returns the section LMA |
| `ALIGN(expr)` | ✅ | One-arg form aligns the absolute location-counter VMA; `ALIGN(0)` / `ALIGN(1)` are no-ops |
| `LENGTH(region)` | ✅ | |
| `ORIGIN(region)` | ✅ | |
| `MIN(a, b)` | ✅ | |
| `MAX(a, b)` | ✅ | |
| Ternary operator (`condition ? a : b`) | ✅ | |
| `DEFINED(sym)` | ✅ | |
| `ABSOLUTE(expr)` | ✅ | Evaluates `expr` as a VMA and forces `SHN_ABS` (kernel `phys_startup_64`) |
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
covered by Wild's integration tests. An x86_64 `vmlinux` link with `--no-gc-sections` matches GNU ld
for `_stext`, `_etext`, `__init_begin`, `_end`, and `.rodata` size. Merge-string inputs are merged at
their section alignment without mixing different alignments in one pool, and `SHF_STRINGS`
tail-merges like GNU ld. `SHF_MERGE` inputs with relocations are concatenated, not unique'd. Constant
pools of different entsize/alignment are kept in separate classes. Merge class starts are padded
to the absolute VMA (GNU ld), so 64-byte crypto tables land on 64-byte addresses. `__sched_class_highest`
can still differ by a few hundred bytes of string-pool packing inside `.rodata`; later symbols match
because `.data..ro_after_init` is 4KiB-aligned.

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
| `SORT_BY_ALIGNMENT(...)` | ✅ | Descending `sh_addralign`, then name (kernel `.data..hot.*`) |
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
| `--build-id` into `*(.note.*)` | ✅ | Merged into the matching output section (kernel `.notes`); not a leftover `PT_LOAD` |

## Known gaps / follow-ups

These are tracked here so they are not forgotten. They are not part of the current layout /
`elf_writer` module split.

### Lower priority versus GNU

* RELA header interleaving after `--emit-relocs` targets
* `.strtab` suffix sharing
* `--build-id` blake3 versus SHA-1 (do not change unless asked)
