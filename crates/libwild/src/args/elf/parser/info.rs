use super::super::DynamicLinker;
use super::super::*;
use crate::arch::SUPPORTED_TARGETS;
use crate::args::ArgumentParser;
use crate::args::BSymbolicKind;
use crate::args::HasCommonArgs as _;
use crate::args::UnresolvedSymbols;
use crate::args::VersionMode;
use crate::args::parse_number;
use crate::bail;
use crate::error::Context as _;
use crate::error::Result;
use hashbrown::HashSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;

pub(crate) fn add_info_and_script_flags(parser: &mut ArgumentParser<ElfArgs>) {
    parser
        .declare()
        .long("help")
        .help("Show this help message")
        .execute(|_args, _modifier_stack| {
            use std::io::Write as _;
            let parser = super::setup_argument_parser();
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", parser.generate_help())?;

            // The following listing is something autoconf detection relies on.
            writeln!(stdout, "wild: supported targets: {SUPPORTED_TARGETS}")?;
            writeln!(
                stdout,
                "wild: supported emulations: {}",
                super::super::supported_emulations()
            )?;

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
            args.dynamic_linker = DynamicLinker::Explicit(Box::from(Path::new(value)));
            Ok(())
        });

    parser
        .declare()
        .long("no-dynamic-linker")
        .help("Omit the load-time dynamic linker request")
        .execute(|args, _modifier_stack| {
            args.dynamic_linker = DynamicLinker::Omit;
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
}
