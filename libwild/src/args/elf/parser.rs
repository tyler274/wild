use super::*;
use crate::alignment::Alignment;
use crate::arch::Architecture;
use crate::arch::SUPPORTED_EMULATIONS;
use crate::arch::SUPPORTED_TARGETS;
use crate::args::ArgumentParser;
use crate::args::BSymbolicKind;
use crate::args::CopyRelocations;
use crate::args::CopyRelocationsDisabledReason;
use crate::args::FileReplacementMode;
use crate::args::Input;
use crate::args::InputSpec;
use crate::args::RelocationModel;
use crate::args::UnresolvedSymbols;
use crate::args::VersionMode;
use crate::args::parse_number;
use crate::bail;
use crate::error::Context as _;
use crate::error::Result;
use crate::linker_script::maybe_forced_sysroot;
use crate::platform::Args as _;
use hashbrown::HashSet;
use object::Endianness;
use object::elf::GNU_PROPERTY_X86_ISA_1_BASELINE;
use object::elf::GNU_PROPERTY_X86_ISA_1_V2;
use object::elf::GNU_PROPERTY_X86_ISA_1_V3;
use object::elf::GNU_PROPERTY_X86_ISA_1_V4;
use std::ffi::CString;
use std::num::NonZero;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

// These flags don't currently affect our behaviour. TODO: Assess whether we should error or warn if
// these are given. This is tricky though. On the one hand we want to be a drop-in replacement for
// other linkers. On the other, we should perhaps somehow let the user know that we don't support a
// feature.
pub(super) const SILENTLY_IGNORED_FLAGS: &[&str] = &[
    // Just like other modern linkers, we don't need groups in order to resolve cycles.
    "start-group",
    "end-group",
    // TODO: This is supposed to suppress built-in search paths, but I don't think we have any
    // built-in search paths. Perhaps we should?
    "nostdlib",
    // TODO
    "no-undefined-version",
    "fatal-warnings",
    "color-diagnostics",
    "undefined-version",
    "sort-common",
    "stats",
    "verbose",
    // Kernel vmlinux.lds / Makefile flags that we do not implement yet.
    "no-warn-rwx-segments",
    "warn-rwx-segments",
];
const SILENTLY_IGNORED_SHORT_FLAGS: &[&str] = &[
    "(",
    ")",
    // On Illumos, the Clang driver inserts a meaningless -C flag before calling any non-GNU ld
    // linker.
    #[cfg(target_os = "illumos")]
    "C",
];

pub(super) const IGNORED_FLAGS: &[&str] = &[
    "fix-cortex-a53-835769",
    "fix-cortex-a53-843419",
    "discard-all",
    "x", // alias for --discard-all
];

// These flags map to the default behavior of the linker.
const DEFAULT_FLAGS: &[&str] = &[
    "no-call-graph-profile-sort",
    "no-copy-dt-needed-entries",
    "no-add-needed",
    "discard-locals",
    "no-fatal-warnings",
];
const DEFAULT_SHORT_FLAGS: &[&str] = &[
    "X", // alias for --discard-locals
];

pub(super) fn setup_argument_parser() -> ArgumentParser<ElfArgs> {
    let mut parser = ArgumentParser::<ElfArgs>::new();

    parser
        .declare_with_param()
        .prefix("L")
        .help("Add directory to library search path")
        .execute(|args, _modifier_stack, value| {
            let handle_sysroot = |path| {
                args.sysroot
                    .as_ref()
                    .and_then(|sysroot| maybe_forced_sysroot(path, sysroot))
                    .unwrap_or_else(|| Box::from(path))
            };

            let dir = handle_sysroot(Path::new(value));
            args.common_mut().save_dir.handle_file(value);
            args.lib_search_path.push(dir);
            Ok(())
        });

    parser
        .declare_with_param()
        .prefix("l")
        .help("Link with library")
        .sub_option_with_value(
            ":filename",
            "Link with specific file",
            |args, modifier_stack, value| {
                let stripped = value.strip_prefix(':').unwrap_or(value);
                let spec = InputSpec::File(Box::from(Path::new(stripped)));
                args.common_mut().inputs.push(Input {
                    spec,
                    search_first: None,
                    modifiers: *modifier_stack.last().unwrap(),
                });
                Ok(())
            },
        )
        .sub_option_with_value(
            "libname",
            "Link with library libname.so or libname.a",
            |args, modifier_stack, value| {
                let spec = InputSpec::Lib(Box::from(value));
                args.common_mut().inputs.push(Input {
                    spec,
                    search_first: None,
                    modifiers: *modifier_stack.last().unwrap(),
                });
                Ok(())
            },
        )
        .execute(|args, modifier_stack, value| {
            let spec = if let Some(stripped) = value.strip_prefix(':') {
                InputSpec::Search(Box::from(stripped))
            } else {
                InputSpec::Lib(Box::from(value))
            };
            args.common_mut().inputs.push(Input {
                spec,
                search_first: None,
                modifiers: *modifier_stack.last().unwrap(),
            });
            Ok(())
        });

    parser
        .declare_with_param()
        .prefix("u")
        .help("Force resolution of the symbol")
        .execute(|args, _modifier_stack, value| {
            args.undefined.push(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .prefix("m")
        .help("Set target architecture")
        .sub_option("elf_x86_64", "x86-64 ELF target", |args, _| {
            args.arch = Architecture::X86_64;
            Ok(())
        })
        .sub_option(
            "elf_x86_64_sol2",
            "x86-64 ELF target (Solaris)",
            |args, _| {
                if args.dynamic_linker.is_none() {
                    args.dynamic_linker = Some(Path::new("/lib/amd64/ld.so.1").into());
                }
                args.arch = Architecture::X86_64;
                Ok(())
            },
        )
        .sub_option("aarch64elf", "AArch64 ELF target", |args, _| {
            args.arch = Architecture::AArch64;
            Ok(())
        })
        .sub_option("aarch64linux", "AArch64 ELF target (Linux)", |args, _| {
            args.arch = Architecture::AArch64;
            Ok(())
        })
        .sub_option("elf64lriscv", "RISC-V 64-bit ELF target", |args, _| {
            args.arch = Architecture::RiscV64;
            Ok(())
        })
        .sub_option(
            "elf64loongarch",
            "LoongArch 64-bit ELF target",
            |args, _| {
                args.arch = Architecture::LoongArch64;
                Ok(())
            },
        )
        .sub_option("elf64lppc", "PowerPC64 LE ELF target", |args, _| {
            args.arch = Architecture::Ppc64;
            Ok(())
        })
        .execute(|_args, _modifier_stack, value| {
            bail!("-m {value} is not yet supported");
        });

    parser
        .declare_with_param()
        .prefix("z")
        .help("Linker option")
        .sub_option("now", "Resolve all symbols immediately", |_, _| Ok(()))
        .sub_option(
            "origin",
            "Mark object as requiring immediate $ORIGIN",
            |args, _| {
                args.needs_origin_handling = true;
                Ok(())
            },
        )
        .sub_option("relro", "Enable RELRO program header", |args, _| {
            args.relro = true;
            Ok(())
        })
        .sub_option("norelro", "Disable RELRO program header", |args, _| {
            args.relro = false;
            Ok(())
        })
        .sub_option("notext", "Do not report DT_TEXTREL as an error", |_, _| {
            Ok(())
        })
        .sub_option("nostart-stop-gc", "Disable start/stop symbol GC", |_, _| {
            Ok(())
        })
        .sub_option(
            "execstack",
            "Mark object as requiring an executable stack",
            |args, _| {
                args.execstack = true;
                Ok(())
            },
        )
        .sub_option(
            "noexecstack",
            "Mark object as not requiring an executable stack",
            |args, _| {
                args.execstack = false;
                Ok(())
            },
        )
        .sub_option("nocopyreloc", "Disable copy relocations", |args, _| {
            args.copy_relocations =
                CopyRelocations::Disallowed(CopyRelocationsDisabledReason::Flag);
            Ok(())
        })
        .sub_option(
            "nodelete",
            "Mark shared object as non-deletable",
            |args, _| {
                args.needs_nodelete_handling = true;
                Ok(())
            },
        )
        .sub_option(
            "defs",
            "Report unresolved symbol references when writing shared object",
            |args, _| {
                args.no_undefined = Some(true);
                Ok(())
            },
        )
        .sub_option(
            "undefs",
            "Do not report unresolved symbol references when writing shared object",
            |args, _| {
                args.no_undefined = Some(false);
                Ok(())
            },
        )
        .sub_option("muldefs", "Allow multiple definitions", |args, _| {
            args.allow_multiple_definitions = true;
            Ok(())
        })
        .sub_option("lazy", "Use lazy binding (default)", |_, _| Ok(()))
        .sub_option(
            "interpose",
            "Mark object to interpose all DSOs but executable",
            |args, _| {
                args.z_interpose = true;
                Ok(())
            },
        )
        .sub_option_with_value(
            "stack-size=",
            "Set size of stack segment",
            |args, _, value| {
                let size: u64 = parse_number(value)?;
                args.z_stack_size = NonZero::new(size);

                Ok(())
            },
        )
        .sub_option(
            "pack-relative-relocs",
            "Pack relative relocations into SHT_RELR",
            |args, _| {
                args.z_pack_relative_relocs = true;
                Ok(())
            },
        )
        .sub_option(
            "nopack-relative-relocs",
            "Do not pack relative relocations into SHT_RELR (default)",
            |args, _| {
                args.z_pack_relative_relocs = false;
                Ok(())
            },
        )
        .sub_option(
            "x86-64-baseline",
            "Mark x86-64-baseline ISA as needed",
            |args, _| {
                args.z_isa = NonZero::new(GNU_PROPERTY_X86_ISA_1_BASELINE);
                Ok(())
            },
        )
        .sub_option("x86-64-v2", "Mark x86-64-v2 ISA as needed", |args, _| {
            args.z_isa = NonZero::new(GNU_PROPERTY_X86_ISA_1_V2);
            Ok(())
        })
        .sub_option("x86-64-v3", "Mark x86-64-v3 ISA as needed", |args, _| {
            args.z_isa = NonZero::new(GNU_PROPERTY_X86_ISA_1_V3);
            Ok(())
        })
        .sub_option("x86-64-v4", "Mark x86-64-v4 ISA as needed", |args, _| {
            args.z_isa = NonZero::new(GNU_PROPERTY_X86_ISA_1_V4);
            Ok(())
        })
        .sub_option_with_value(
            "max-page-size=",
            "Set maximum page size for load segments",
            |args, _, value| {
                let size: u64 = parse_number(value)?;
                if !size.is_power_of_two() {
                    bail!("Invalid alignment {size:#x}");
                }
                args.max_page_size = Some(Alignment {
                    exponent: size.trailing_zeros() as u8,
                });

                Ok(())
            },
        )
        .execute(|args, _modifier_stack, value| {
            args.warn_unsupported(&(format!("-z {value}")))?;
            Ok(())
        });

    parser
        .declare_with_param()
        .prefix("R")
        .help("Add runtime library search path")
        .execute(|args, _modifier_stack, value| {
            if Path::new(value).is_file() {
                args.common_mut()
                    .unrecognized_options
                    .push(format!("-R,{value}(filename)"));
            } else {
                args.rpath_set.insert(value.to_string());
            }
            Ok(())
        });

    parser
        .declare_with_param()
        .prefix("O")
        .execute(|_args, _modifier_stack, _value|
        // We don't use opt-level for now.
        Ok(()));

    parser
        .declare()
        .long("static")
        .long("Bstatic")
        .help("Disallow linking of shared libraries")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().allow_shared = false;
            Ok(())
        });

    parser
        .declare()
        .long("Bdynamic")
        .help("Allow linking of shared libraries")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().allow_shared = true;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("output")
        .prefix("o")
        .help("Set the output filename")
        .execute(|args, _modifier_stack, value| {
            args.common.output = Arc::from(Path::new(value));
            Ok(())
        });

    parser
        .declare()
        .long("strip-all")
        .short("s")
        .help("Strip all symbols")
        .execute(|args, _modifier_stack| {
            args.strip = Strip::All;
            Ok(())
        });

    parser
        .declare()
        .long("strip-debug")
        .short("S")
        .help("Strip debug symbols")
        .execute(|args, _modifier_stack| {
            args.strip = Strip::Debug;
            Ok(())
        });

    parser
        .declare()
        .long("gc-sections")
        .help("Enable removal of unused sections")
        .execute(|args, _modifier_stack| {
            args.gc_sections = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-gc-sections")
        .help("Disable removal of unused sections")
        .execute(|args, _modifier_stack| {
            args.gc_sections = false;
            Ok(())
        });

    parser
        .declare()
        .long("shared")
        .long("Bshareable")
        .help("Create a shared library")
        .execute(|args, _modifier_stack| {
            args.should_output_executable = false;
            Ok(())
        });

    parser
        .declare()
        .long("pie")
        .long("pic-executable")
        .help("Create a position-independent executable")
        .execute(|args, _modifier_stack| {
            args.common.relocation_model = RelocationModel::PositionIndependent;
            args.should_output_executable = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-pie")
        .help("Do not create a position-independent executable (default)")
        .execute(|args, _modifier_stack| {
            args.common.relocation_model = RelocationModel::Fixed;
            args.should_output_executable = true;
            Ok(())
        });

    parser
        .declare()
        .short("r")
        .long("relocatable")
        .help("Create a relocatable object file")
        .execute(|args, _modifier_stack| {
            args.should_output_executable = false;
            args.should_output_partial_object = true;
            args.gc_sections = false;
            args.relro = false;
            args.should_write_linker_identity = false;
            args.merge_sections = false;
            Ok(())
        });

    parser
        .declare()
        .short("q")
        .long("emit-relocs")
        .help("Leave relocation sections in fully linked output")
        .execute(|args, _modifier_stack| {
            args.emit_relocs = true;
            Ok(())
        });

    parser
        .declare()
        .long("discard-none")
        .help("Do not discard any local symbols")
        .execute(|args, _modifier_stack| {
            args.discard_none = true;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("pack-dyn-relocs")
        .help("Specify dynamic relocation packing format")
        .execute(|args, _modifier_stack, value| {
            match value {
                "none" => args.pack_dyn_relocs = PackDynRelocs::None,
                "relr" => args.pack_dyn_relocs = PackDynRelocs::Relr,
                "android" => args.pack_dyn_relocs = PackDynRelocs::Android,
                "android+relr" => args.pack_dyn_relocs = PackDynRelocs::AndroidRelr,
                value => {
                    args.warn_unsupported(&format!("--pack-dyn-relocs={value}"))?;
                }
            }
            Ok(())
        });

    parser
        .declare()
        .long("help")
        .help("Show this help message")
        .execute(|_args, _modifier_stack| {
            use std::io::Write as _;
            let parser = setup_argument_parser();
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", parser.generate_help())?;

            // The following listing is something autoconf detection relies on.
            writeln!(stdout, "wild: supported targets: {SUPPORTED_TARGETS}")?;
            writeln!(stdout, "wild: supported emulations: {SUPPORTED_EMULATIONS}")?;

            std::process::exit(0);
        });

    parser
        .declare()
        .long("version")
        .help("Show version information and exit")
        .execute(|args, _modifier_stack| {
            args.common.version_mode = VersionMode::ExitAfterPrint;
            Ok(())
        });

    parser
        .declare()
        .short("v")
        .help("Print version and continue linking if object files are specified")
        .execute(|args, _modifier_stack| {
            args.common.version_mode = VersionMode::Verbose;
            Ok(())
        });

    parser
        .declare()
        .short("V")
        .help("Print version along with supported emulations and continue linking if object files are specified")
        .execute(|args, _modifier_stack| {
            args.common.version_mode = VersionMode::VerboseWithEmulations;
            Ok(())
        });

    parser
        .declare()
        .long("demangle")
        .help("Enable symbol demangling")
        .execute(|args, _modifier_stack| {
            args.common_mut().demangle = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-demangle")
        .help("Disable symbol demangling")
        .execute(|args, _modifier_stack| {
            args.common_mut().demangle = false;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("dynamic-linker")
        .help("Set dynamic linker path")
        .execute(|args, _modifier_stack, value| {
            args.dynamic_linker = Some(Box::from(Path::new(value)));
            Ok(())
        });

    parser
        .declare()
        .long("no-dynamic-linker")
        .help("Omit the load-time dynamic linker request")
        .execute(|args, _modifier_stack| {
            args.dynamic_linker = None;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("entry")
        .short("e")
        .help("Set the entry point")
        .execute(|args, _modifier_stack, value| {
            args.entry = Some(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("wild-experiments")
        .help("List of numbers. Used to tweak internal parameters. '_' keeps default value.")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().numeric_experiments = value
                .split(',')
                .map(|p| {
                    if p == "_" {
                        Ok(None)
                    } else {
                        Ok(Some(p.parse()?))
                    }
                })
                .collect::<Result<Vec<Option<u64>>>>()?;
            Ok(())
        });

    parser
        .declare()
        .long("as-needed")
        .help("Set DT_NEEDED if used")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().as_needed = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-as-needed")
        .help("Always set DT_NEEDED")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().as_needed = false;
            Ok(())
        });

    parser
        .declare()
        .long("whole-archive")
        .help("Include all objects from archives")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().whole_archive = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-whole-archive")
        .help("Disable --whole-archive")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().whole_archive = false;
            Ok(())
        });

    parser
        .declare()
        .long("push-state")
        .help("Save current linker flags")
        .execute(|_args, modifier_stack| {
            modifier_stack.push(*modifier_stack.last().unwrap());
            Ok(())
        });

    parser
        .declare()
        .long("pop-state")
        .help("Restore previous linker flags")
        .execute(|_args, modifier_stack| {
            modifier_stack.pop();
            if modifier_stack.is_empty() {
                bail!("Mismatched --pop-state");
            }
            Ok(())
        });

    parser
        .declare()
        .long("eh-frame-hdr")
        .help("Create .eh_frame_hdr section")
        .execute(|args, _modifier_stack| {
            args.should_write_eh_frame_hdr = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-eh-frame-hdr")
        .help("Don't create .eh_frame_hdr section")
        .execute(|args, _modifier_stack| {
            args.should_write_eh_frame_hdr = false;
            Ok(())
        });

    parser
        .declare()
        .long("export-dynamic")
        .short("E")
        .help("Export all dynamic symbols")
        .execute(|args, _modifier_stack| {
            args.export_all_dynamic_symbols = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-export-dynamic")
        .help("Do not export dynamic symbols")
        .execute(|args, _modifier_stack| {
            args.export_all_dynamic_symbols = false;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("compress-debug-sections")
        .help("Compress debug sections using zlib or zstd")
        .execute(|args, _modifier_stack, value| {
            match value {
                "none" => args.debug_compression_kind = None,
                "zlib" | "zlib-gabi" => args.debug_compression_kind = Some(CompressionKind::Zlib),
                "zstd" => args.debug_compression_kind = Some(CompressionKind::Zstd),
                value => bail!("--compress-debug-sections={value}"),
            }
            Ok(())
        });

    parser
        .declare()
        .long("gdb-index")
        .help("Generate GDB index")
        .execute(|args, _modifier_stack| {
            args.gdb_index = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-gdb-index")
        .help("Disable GDB index generation")
        .execute(|args, _modifier_stack| {
            args.gdb_index = false;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("soname")
        .prefix("h")
        .help("Set shared object name")
        .execute(|args, _modifier_stack, value| {
            args.soname = Some(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("rpath")
        .help("Add directory to runtime library search path")
        .execute(|args, _modifier_stack, value| {
            args.rpath_set.insert(value.to_string());
            Ok(())
        });

    parser
        .declare()
        .long("no-string-merge")
        .help("Disable section merging")
        .execute(|args, _modifier_stack| {
            args.merge_sections = false;
            Ok(())
        });

    parser
        .declare()
        .long("no-undefined")
        .help("Do not allow unresolved symbols in object files")
        .execute(|args, _modifier_stack| {
            args.no_undefined = Some(true);
            Ok(())
        });

    parser
        .declare()
        .long("allow-multiple-definition")
        .help("Allow multiple definitions of symbols")
        .execute(|args, _modifier_stack| {
            args.allow_multiple_definitions = true;
            Ok(())
        });

    parser
        .declare()
        .long("relax")
        .help("Enable target-specific optimization (instruction relaxation)")
        .execute(|args, _modifier_stack| {
            args.relax = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-relax")
        .help("Disable relaxation")
        .execute(|args, _modifier_stack| {
            args.relax = false;
            Ok(())
        });

    parser
        .declare()
        .long("got-plt-syms")
        .help("Write symbol table entries that point to the GOT/PLT entry for symbols")
        .execute(|args, _modifier_stack| {
            args.got_plt_syms = true;
            Ok(())
        });

    parser
        .declare()
        .long("Bsymbolic")
        .help("Bind global references locally")
        .execute(|args, _modifier_stack| {
            args.b_symbolic = BSymbolicKind::All;
            Ok(())
        });

    parser
        .declare()
        .long("Bsymbolic-functions")
        .help("Bind global function references locally")
        .execute(|args, _modifier_stack| {
            args.b_symbolic = BSymbolicKind::Functions;
            Ok(())
        });

    parser
        .declare()
        .long("Bsymbolic-non-weak-functions")
        .help("Bind non-weak global function references locally")
        .execute(|args, _modifier_stack| {
            args.b_symbolic = BSymbolicKind::NonWeakFunctions;
            Ok(())
        });

    parser
        .declare()
        .long("Bsymbolic-non-weak")
        .help("Bind non-weak global references locally")
        .execute(|args, _modifier_stack| {
            args.b_symbolic = BSymbolicKind::NonWeak;
            Ok(())
        });

    parser
        .declare()
        .long("Bno-symbolic")
        .help("Do not bind global symbol references locally")
        .execute(|args, _modifier_stack| {
            args.b_symbolic = BSymbolicKind::None;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("thread-count")
        .help("Set the number of threads to use")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().num_threads = Some(NonZeroUsize::try_from(value.parse::<usize>()?)?);
            Ok(())
        });

    parser
        .declare_with_param()
        .long("exclude-libs")
        .help("Exclude libraries")
        .execute(|args, _modifier_stack, value| {
            for lib in value.split([',', ':']) {
                if lib.is_empty() {
                    continue;
                }

                if lib == "ALL" {
                    args.exclude_libs = ExcludeLibs::All;
                    return Ok(());
                }

                match &mut args.exclude_libs {
                    ExcludeLibs::All => {}
                    ExcludeLibs::None => {
                        let mut set = HashSet::new();
                        set.insert(Box::from(lib));
                        args.exclude_libs = ExcludeLibs::Some(set);
                    }
                    ExcludeLibs::Some(set) => {
                        set.insert(Box::from(lib));
                    }
                }
            }

            Ok(())
        });

    parser
        .declare_with_param()
        .long("version-script")
        .help("Use version script")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().save_dir.handle_file(value);
            args.version_script_path = Some(PathBuf::from(value));
            Ok(())
        });

    parser
        .declare_with_param()
        .long("script")
        .prefix("T")
        .help("Use linker script")
        .execute(|args, _modifier_stack, value| {
            // -Ttext=ADDR, -Tdata=ADDR, -Tbss=ADDR are segment start overrides,
            // not linker script paths. Handle them here since they share the -T prefix.
            // The prefix handler gives us the part after "-T", which may be:
            //   "text=0x700000"  (from -Ttext=0x700000)
            // We only handle the "name=ADDR" form here.
            if let Some(addr) = value.strip_prefix("text=") {
                args.ttext = Some(
                    parse_number(addr)
                        .with_context(|| format!("Invalid address `{addr}` in -Ttext"))?,
                );
                return Ok(());
            }
            if let Some(addr) = value.strip_prefix("data=") {
                args.tdata = Some(
                    parse_number(addr)
                        .with_context(|| format!("Invalid address `{addr}` in -Tdata"))?,
                );
                return Ok(());
            }
            if let Some(addr) = value.strip_prefix("bss=") {
                args.tbss = Some(
                    parse_number(addr)
                        .with_context(|| format!("Invalid address `{addr}` in -Tbss"))?,
                );
                return Ok(());
            }
            args.common_mut().save_dir.handle_file(value);
            args.common_mut().add_script(value);
            Ok(())
        });

    parser
        .declare_with_param()
        .long("export-dynamic-symbol")
        .help("Export dynamic symbol")
        .execute(|args, _modifier_stack, value| {
            args.export_list.push(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("export-dynamic-symbol-list")
        .help("Export dynamic symbol list")
        .execute(|args, _modifier_stack, value| {
            args.export_list_path = Some(PathBuf::from(value));
            Ok(())
        });

    parser
        .declare_with_param()
        .long("dynamic-list")
        .help("Read the dynamic symbol list from a file")
        .execute(|args, _modifier_stack, value| {
            args.b_symbolic = BSymbolicKind::All;
            args.export_list_path = Some(PathBuf::from(value));
            Ok(())
        });

    parser
        .declare_with_param()
        .long("write-gc-stats")
        .help("Write GC statistics")
        .execute(|args, _modifier_stack, value| {
            args.write_gc_stats = Some(PathBuf::from(value));
            Ok(())
        });

    parser
        .declare_with_param()
        .long("gc-stats-ignore")
        .help("Ignore files in GC stats")
        .execute(|args, _modifier_stack, value| {
            args.gc_stats_ignore.push(value.to_owned());
            Ok(())
        });

    parser
        .declare()
        .long("no-identity-comment")
        .help("Don't write the linker name and version in .comment")
        .execute(|args, _modifier_stack| {
            args.should_write_linker_identity = false;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("debug-fuel")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().debug_fuel = Some(AtomicI64::new(value.parse()?));
            args.common_mut().num_threads = Some(NonZeroUsize::new(1).unwrap());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("unresolved-symbols")
        .help("Specify how to handle unresolved symbols")
        .execute(|args, _modifier_stack, value| {
            args.unresolved_symbols = match value {
                "report-all" => UnresolvedSymbols::ReportAll,
                "ignore-in-shared-libs" => UnresolvedSymbols::IgnoreInSharedLibs,
                "ignore-in-object-files" => UnresolvedSymbols::IgnoreInObjectFiles,
                "ignore-all" => UnresolvedSymbols::IgnoreAll,
                _ => bail!("Invalid unresolved-symbols value {value}"),
            };
            Ok(())
        });

    parser
        .declare_with_param()
        .long("undefined")
        .help("Force resolution of the symbol")
        .execute(|args, _modifier_stack, value| {
            args.undefined.push(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("wrap")
        .help("Use a wrapper function")
        .execute(|args, _modifier_stack, value| {
            args.wrap.push(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("defsym")
        .help("Define a symbol alias: --defsym=symbol=value")
        .execute(|args, _modifier_stack, value| {
            let parts: Vec<&str> = value.splitn(2, '=').collect();
            if parts.len() != 2 {
                bail!("Invalid --defsym format. Expected: --defsym=symbol=value");
            }
            let symbol_name = parts[0].to_owned();
            let value_str = parts[1].to_owned();

            args.defsym.push((symbol_name, value_str));
            Ok(())
        });

    parser
        .declare_with_param()
        .long("section-start")
        .help("Set start address for a section: --section-start=.section=address")
        .execute(|args, _modifier_stack, value| {
            let parts: Vec<&str> = value.splitn(2, '=').collect();
            if parts.len() != 2 {
                bail!("Invalid --section-start format. Expected: --section-start=.section=address");
            }

            let section_name = parts[0].to_owned();
            let address = parse_number(parts[1]).with_context(|| {
                format!(
                    "Invalid address `{}` in --section-start={}",
                    parts[1], value
                )
            })?;
            args.section_start
                .insert(section_name.into_bytes(), address);

            Ok(())
        });

    parser
        .declare_with_param()
        .long("hash-style")
        .help("Set hash style")
        .execute(|args, _modifier_stack, value| {
            args.hash_style = match value {
                "gnu" => HashStyle::Gnu,
                "sysv" => HashStyle::Sysv,
                "both" => HashStyle::Both,
                _ => bail!("Unknown hash-style `{value}`"),
            };
            Ok(())
        });

    parser
        .declare()
        .long("enable-new-dtags")
        .help("Use DT_RUNPATH and DT_FLAGS/DT_FLAGS_1 (default)")
        .execute(|args, _modifier_stack| {
            args.enable_new_dtags = true;
            Ok(())
        });

    parser
        .declare()
        .long("disable-new-dtags")
        .help("Use DT_RPATH and individual dynamic entries instead of DT_FLAGS")
        .execute(|args, _modifier_stack| {
            args.enable_new_dtags = false;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("retain-symbols-file")
        .help(
            "Filter symtab to contain only symbols listed in the supplied file. \
            One symbol per line.",
        )
        .execute(|args, _modifier_stack, value| {
            // The performance this flag is not especially optimised. For one, we copy each string
            // to the heap. We also do two lookups in the hashset for each symbol. This is a pretty
            // obscure flag that we don't expect to be used much, so at this stage, it doesn't seem
            // worthwhile to optimise it.
            let contents = std::fs::read_to_string(value)
                .with_context(|| format!("Failed to read `{value}`"))?;
            args.strip = Strip::Retain(
                contents
                    .lines()
                    .filter_map(|l| {
                        if l.is_empty() {
                            None
                        } else {
                            Some(l.as_bytes().to_owned())
                        }
                    })
                    .collect(),
            );
            Ok(())
        });

    parser
        .declare_with_param()
        .long("build-id")
        .help("Generate build ID")
        .execute(|args, _modifier_stack, value| {
            args.build_id = match value {
                "none" => BuildIdOption::None,
                "fast" | "md5" | "sha1" => BuildIdOption::Fast,
                "uuid" => BuildIdOption::Uuid,
                s if s.starts_with("0x") || s.starts_with("0X") => {
                    let hex_string = &s[2..];
                    let decoded_bytes = hex::decode(hex_string)
                        .with_context(|| format!("Invalid Hex Build Id `0x{hex_string}`"))?;
                    BuildIdOption::Hex(decoded_bytes)
                }
                s => bail!(
                    "Invalid build-id value `{s}` valid values are `none`, `fast`, `md5`, `sha1` and `uuid`"
                ),
            };
            Ok(())
        });

    parser
        .declare_with_param()
        .long("icf")
        .help("Enable identical code folding (merge duplicate functions)")
        .execute(|args, _modifier_stack, value| {
            match value {
                "none" => {}
                other => args.warn_unsupported(&format!("--icf={other}"))?,
            }
            Ok(())
        });

    parser
        .declare_with_param()
        .long("sort-section")
        .help("Specify section sorting criteria")
        .execute(|args, _modifier_stack, value| {
            args.sort_section = Some(match value {
                "name" => SortSectionMode::Name,
                "alignment" => SortSectionMode::Alignment,
                other => {
                    args.warn_unsupported(&format!("--sort-section={other}"))?;
                    return Ok(());
                }
            });
            Ok(())
        });

    parser
        .declare_with_param()
        .long("sysroot")
        .help("Set system root")
        .execute(|args, _modifier_stack, value| {
            args.common_mut().save_dir.handle_file(value);
            let sysroot = std::fs::canonicalize(value).unwrap_or_else(|_| PathBuf::from(value));
            args.sysroot = Some(Box::from(sysroot.as_path()));
            for path in &mut args.lib_search_path {
                if let Some(new_path) = maybe_forced_sysroot(path, &sysroot) {
                    *path = new_path;
                }
            }
            Ok(())
        });

    parser
        .declare_with_param()
        .long("auxiliary")
        .short("f")
        .help("Set DT_AUXILIARY to a given value")
        .execute(|args, _modifier_stack, value| {
            args.auxiliary.push(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("plugin-opt")
        .help("Pass options to the plugin")
        .execute(|args, _modifier_stack, value| {
            args.plugin_args
                .push(CString::new(value).context("Invalid --plugin-opt argument")?);
            Ok(())
        });

    parser
        .declare_with_param()
        .long("dependency-file")
        .help("Write dependency rules")
        .execute(|args, _modifier_stack, value| {
            args.dependency_file = Some(PathBuf::from(value));
            Ok(())
        });

    parser
        .declare()
        .short("t")
        .long("trace")
        .help("Print opened input files")
        .execute(|args, _modifier_stack| {
            args.trace = true;
            Ok(())
        });

    parser
        .declare_with_param()
        .long("plugin")
        .help("Load plugin")
        .execute(|args, _modifier_stack, value| {
            args.plugin_path = Some(value.to_owned());
            Ok(())
        });

    parser
        .declare_with_param()
        .long("rpath-link")
        .help("Add runtime library search path")
        .execute(|_args, _modifier_stack, _value| {
            // TODO
            Ok(())
        });

    parser
        .declare()
        .long("start-lib")
        .help("Start library group")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().archive_semantics = true;
            Ok(())
        });

    parser
        .declare()
        .long("end-lib")
        .help("End library group")
        .execute(|_args, modifier_stack| {
            modifier_stack.last_mut().unwrap().archive_semantics = false;
            Ok(())
        });

    parser
        .declare()
        .long("no-update-in-place")
        .help("Delete and recreate the file")
        .execute(|args, _modifier_stack| {
            args.common_mut().file_replacement_mode = Some(FileReplacementMode::UnlinkAndReplace);
            Ok(())
        });

    parser
        .declare()
        .short("EL")
        .help("Select the little-endian format in the OUTPUT_FORMAT command")
        .execute(|args, _modifier_stack| {
            args.output_format_endian = Some(Endianness::Little);
            Ok(())
        });

    parser
        .declare()
        .short("EB")
        .help("Select the big-endian format in the OUTPUT_FORMAT command")
        .execute(|args, _modifier_stack| {
            args.output_format_endian = Some(Endianness::Big);
            Ok(())
        });

    parser
        .declare()
        .long("prepopulate-maps")
        .help("Prepopulate maps")
        .execute(|args, _modifier_stack| {
            args.common_mut().prepopulate_maps = true;
            Ok(())
        });

    parser
        .declare()
        .long("verbose-gc-stats")
        .help("Show GC statistics")
        .execute(|args, _modifier_stack| {
            args.verbose_gc_stats = true;
            Ok(())
        });

    parser
        .declare()
        .long("allow-shlib-undefined")
        .help("Allow undefined symbol references in shared libraries")
        .execute(|args, _modifier_stack| {
            args.allow_shlib_undefined = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-allow-shlib-undefined")
        .help("Disallow undefined symbol references in shared libraries")
        .execute(|args, _modifier_stack| {
            args.allow_shlib_undefined = false;
            Ok(())
        });

    parser
        .declare()
        .long("error-unresolved-symbols")
        .help("Treat unresolved symbols as errors")
        .execute(|args, _modifier_stack| {
            args.error_unresolved_symbols = true;
            Ok(())
        });

    parser
        .declare()
        .long("warn-unresolved-symbols")
        .help("Treat unresolved symbols as warnings")
        .execute(|args, _modifier_stack| {
            args.error_unresolved_symbols = false;
            Ok(())
        });

    parser
        .declare()
        .long("use-android-relr-tags")
        .help("Use Android version of SHT_RELR and DT_RELR")
        .execute(|args, _modifier_stack| {
            args.use_android_relr_tags = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-use-android-relr-tags")
        .help("Do not use Android version of SHT_RELR and DT_RELR (default)")
        .execute(|args, _modifier_stack| {
            args.use_android_relr_tags = false;
            Ok(())
        });

    parser
        .declare()
        .long("wild-experimental-sframe")
        .help("Enable experimental support for SFrame V2 (this option may be removed at any time)")
        .execute(|args, _modifier_stack| {
            args.experimental_sframe = true;
            Ok(())
        });

    parser
        .declare()
        .short("n")
        .long("nmagic")
        .help("Disable page alignment of sections and disable linking against shared libraries")
        .execute(|args, _modifier_stack| {
            args.nmagic = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-nmagic")
        .help("Page align sections (default)")
        .execute(|args, _modifier_stack| {
            args.nmagic = false;
            Ok(())
        });

    parser
        .declare()
        .long("rosegment")
        .help("Put read-only non-executable sections in their own segment (default)")
        .execute(|args, _modifier_stack| {
            args.rosegment = true;
            Ok(())
        });

    parser
        .declare()
        .long("no-rosegment")
        .help("Don't put read-only non-executable sections in their own segment")
        .execute(|args, _modifier_stack| {
            args.rosegment = false;
            Ok(())
        });

    parser
        .declare()
        .long("discard-sframe")
        .help("Discard SFrame section")
        .execute(|args, _modifier_stack| {
            args.discard_sframe = true;
            Ok(())
        });

    crate::args::declare_common_args(&mut parser);

    add_silently_ignored_flags(&mut parser);
    add_default_flags(&mut parser);

    parser
}

fn add_silently_ignored_flags(parser: &mut ArgumentParser<ElfArgs>) {
    for flag in SILENTLY_IGNORED_FLAGS {
        let mut declaration = parser.declare();
        declaration = declaration.long(flag);
        declaration.execute(|_args, _modifier_stack| Ok(()));
    }
    for flag in SILENTLY_IGNORED_SHORT_FLAGS {
        let mut declaration = parser.declare();
        declaration = declaration.short(flag);
        declaration.execute(|_args, _modifier_stack| Ok(()));
    }
}

fn add_default_flags(parser: &mut ArgumentParser<ElfArgs>) {
    for flag in DEFAULT_FLAGS {
        let mut declaration = parser.declare();
        declaration = declaration.long(flag);
        declaration.execute(|_args, _modifier_stack| Ok(()));
    }
    for flag in DEFAULT_SHORT_FLAGS {
        let mut declaration = parser.declare();
        declaration = declaration.short(flag);
        declaration.execute(|_args, _modifier_stack| Ok(()));
    }
}
