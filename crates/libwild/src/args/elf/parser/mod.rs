mod info;
mod inputs;

use super::*;
use crate::args::ArgumentParser;
use crate::args::FileReplacementMode;
use crate::bail;
use crate::error::Context as _;
use crate::linker_script::maybe_forced_sysroot;
use crate::platform::Args as _;
#[allow(unused_imports)]
pub(crate) use info::*;
#[allow(unused_imports)]
pub(crate) use inputs::*;
use object::Endianness;
use std::ffi::CString;
use std::path::PathBuf;

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
    // GCC 15 / glibc pass these once they detect a GNU ld --version line.
    "no-error-execstack",
    "error-execstack",
    "warn-execstack",
    "no-warn-execstack",
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
    add_search_and_output_flags(&mut parser);
    add_info_and_script_flags(&mut parser);
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
        .long("orphan-handling")
        .help("Control how input sections not mentioned in a linker script are handled")
        .execute(|args, _modifier_stack, value| {
            args.orphan_handling = match value {
                "place" => crate::platform::OrphanHandling::Place,
                "discard" => crate::platform::OrphanHandling::Discard,
                "warn" => crate::platform::OrphanHandling::Warn,
                "error" => crate::platform::OrphanHandling::Error,
                other => bail!(
                    "Invalid --orphan-handling `{other}`, expected place, discard, warn, or error"
                ),
            };
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
