# Design

This document provides a high level overview of Wild's design. The intent is to not go into too much
detail, otherwise we increase the risk that it'll get out-of-sync with the code. For full details,
see comments in the code and the code itself.

## Phases

The linker runs several phases. Each phase borrows data immutably from the previous phases. The high
level phases are:

* `args.rs`: Parse command-line arguments.
* `input_data.rs`: Open input files with mmap and split archives into their separate objects.
* `string_merging.rs`: Strings in string-merge sections are deduplicated.
* `symbol_db.rs`: Build a hashmap from symbol names to symbol IDs.
* `resolution.rs`: Resolve all undefined symbols and in the process decide which archived objects
  will be processed.
* `layout/`:
  * Traverse graph of relocations, in the process, determining which input sections are needed and
    how much space is needed in the various linker-generated sections such as the GOT (global offset
    table), symbol tables, dynamic relocation tables etc.
  * Allocate addresses for sections, symbols, program segments etc.
* `elf_writer/`: Copy input sections to the output file, applying relocations as we go. Write
  linker-generated sections.

For a more detailed look at the phases of the linker, run with the `--time` flag.

## Incremental linking

`--incremental` keeps dense `SymbolId` / `FileId` indexes for the GC graph (those are rebuilt every
link). Cross-run identity is a generational atom table in `incremental/`: unchanged inputs reuse a
handle, a replaced path reuses a slot with a new generation, and reverse-reloc lists plus
resolutions are keyed by `(atom, local symbol)` so a neighboring file cannot reshuffle IDs. Skip
updates merge reverse-reloc lists, replacing sites in rewritten objects and keeping sites in skipped
ones. GC and LTO still fall back to a full padded link. Custom linker scripts skip section padding
so kernel `ASSERT`s keep their sizes; unchanged inputs can still skip payloads.

Integration tests cover GCC, Clang, and rustc at `-O0`/`-O1`/`-O2`/`-O3`/`-Os` (plus LTO fallback)
and an unchanged `--incremental` relink of x86_64 `vmlinux` (`WILD_LINUX_TREE`). Rust `--emit=obj`
keeps a stable `.o` path; rustc save-dir tests allow fallback because codegen-unit hashes change
with `--cfg wild_inc`. Glibc DSOs still fail `--incremental` (debug offset verification on
`libc.so`, leftover `.rela.dyn` / `.relr.dyn` slots on an unchanged `ld.so` / `libm.so` update).

## Threading

The linker makes extensive use of multiple threads. The thread pool is owned by the rayon library.
Where possible, we use functions like rayon's `par_iter` to process collections in parallel. Failing
that, we use `par_bridge` which allows the main thread to create work to send out to the thread
pool. In a couple of cases however, we have graph algorithms that don't fit neatly into rayon's
model. In those cases, we spawn one rayon scoped task per thread and then do job control ourselves.

There are various phases within the linker that are single threaded. This is fine, so long as those
phases run quickly enough.

## Testing

Most testing is done by `integration_tests.rs`. This compiles various programs that are written in
C, C++, Rust and assembly. It then links them with our reference linkers — GNU ld, and for general
ELF cases also LLD and Mold (`ReferenceLinkers:bfd,lld,mold`). It links them with Wild and compares
the resulting binaries using our own custom diff tool, `linker-diff`. Provided that succeeds, it
then executes all the linked programs and checks that they give the correct answer.

A four-way diff (GNU ld, LLD, Mold, Wild) is the default for tests that list all three reference
linkers. Tests that omit `ReferenceLinkers` still use GNU ld only. Set `WILD_FOUR_WAY=1` or
`default_reference_linkers = ["bfd", "lld", "mold"]` in the test config to opt unpinned tests into
the same four-way. Linker-script tests pin `ReferenceLinkers:bfd`; GNU ld is the script oracle.

Kernel `vmlinux` is GNU-only. Set `WILD_LINUX_TREE` to an x86_64 tree that already has `vmlinux.o`
and GNU `vmlinux.unstripped`, then `cargo test -p wild-linker --test integration_tests -- vmlinux`.
Pack objects with `scripts/pack-vmlinux-objects.sh`. CI job `vmlinux` runs when the repository
variable `WILD_LINUX_OBJECTS_URL` points at that tarball (a from-scratch kernel build will not fit
the 10-minute timeout). `vmlinux-incremental` links the same objects with `--incremental` and checks
an unchanged second link records `incremental-update`. Follow-up: a small userspace / initramfs also
linked with Wild.

Glibc's `libc.so` link uses GNU ld's default shared script (`DATA_SEGMENT_*`, `CONSTANT`,
`ONLY_IF_*`). Wild can parse and link that script (see `linker-script-gnu-default`). `nix develop`
unpacks nixpkgs glibc, sets `WILD_GLIBC_TREE` / `WILD_GLIBC_BUILD` / `WILD_GLIBC_HEADERS`, and
provides `wild-build-glibc` (GNU ld + GCC 15). Wild's `--version` first line is GNU ld compatible
so glibc `configure` and the kernel's `scripts/ld-version.sh` accept it; the GNU oracle is still
linked with GNU ld so the relink tests have something to diff. Then
`cargo test -p wild-linker --test integration_tests -- glibc`. Override the env vars to use another
tree. `wild-glibc-check` installs those Wild-linked `libc.so` / `ld.so` / `libm.so` (and other
`lib%.so` relinks when present) into the GNU build and runs a `make test` subset (TLS, IFUNC,
RELR, ctors, malloc, libm, nptl), then restores the GNU oracles. `--incremental` on those DSOs is
follow-up. A full `make check` is still follow-up.

## Modularity (Mold and LLD)

Wild stays one `libwild` crate. The notes below are about *module* boundaries, not new workspace
crates.

Mold is an ELF-first C++ linker. A `Context` holds all state. `src/passes.cc` runs named passes
(resolve, GC, create output sections, LTO, copy). Output is a list of `Chunk` subclasses
(`OutputEhdr`, `OutputSection`, synthetic sections) each with `copy_buf`. Architecture files
(`arch-x86-64.cc` and similar) stay thin. Mach-O is a separate tree, not a shared abstraction.

LLD splits by object format first: `lld/ELF`, `lld/COFF`, `lld/MachO`, `lld/wasm`, plus `lld/Common`.
Each format has its own driver. ELF work lives in `Writer.cpp`, `OutputSections.cpp`,
`InputFiles.cpp`, `Relocations.cpp`, `SyntheticSections.cpp`, and `LinkerScript.cpp` (parse,
evaluate, and orphan insertion together). Arch code sits in `lld/ELF/Arch/`.

Wild already matches the useful parts of both without a god `ctx` or extra crates:

* Format modules (`elf/`, `wasm/`, `macho/`) plus a `Platform` trait (static dispatch) — LLD's
  format split, Mold's lack of virtual `Platform`.
* Phase modules (`args`, `input_data`, `resolution`, `layout/`, `elf_writer/`) — Mold's passes, but
  each phase borrows the previous instead of mutating one context.
* `linker_script/parse` is separate from `layout/script.rs` and output-order in `elf/abi.rs`. LLD
  keeps script parse and orphan placement in one file; keep them apart here.
* `OutputSectionId` / `PartId` plus `elf_writer` section writers play the role of Mold's `Chunk`.
* Arch files (`elf_x86_64.rs`, …) stay thin, like both.

Do not name child modules `layout`, `platform`, or `object` (they shadow parent modules). Keep
`crate::elf::*` / `crate::wasm::*` re-exports. Further splits should stay inside those trees.
