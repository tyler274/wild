use super::super::*;
use crate::alignment::Alignment;
use crate::arch::Architecture;
use crate::args::ArgumentParser;
use crate::args::CopyRelocations;
use crate::args::CopyRelocationsDisabledReason;
use crate::args::Input;
use crate::args::InputSpec;
use crate::args::RelocationModel;
use crate::args::parse_number;
use crate::bail;
use crate::linker_script::maybe_forced_sysroot;
use crate::platform::Args as _;
use object::elf::GNU_PROPERTY_X86_ISA_1_BASELINE;
use object::elf::GNU_PROPERTY_X86_ISA_1_V2;
use object::elf::GNU_PROPERTY_X86_ISA_1_V3;
use object::elf::GNU_PROPERTY_X86_ISA_1_V4;
use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;

pub(crate) fn add_search_and_output_flags(parser: &mut ArgumentParser<ElfArgs>) {
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
}
