//! Opt-in x86_64 `vmlinux` link against a prebuilt kernel tree.
//!
//! Set `WILD_LINUX_TREE` to a tree that already has `vmlinux.o`, the extra
//! objects from a `vmlinux` link, `arch/x86/kernel/vmlinux.lds`, and GNU
//! `vmlinux.unstripped`. Skipped when the variable is unset (CI cannot compile
//! a kernel in 10 minutes).

use crate::Filter;
use crate::build_dir;
use crate::wild_path;
use libtest_mimic::Trial;
use libwild::bail;
use libwild::error::Context as _;
use libwild::error::Result;
use object::Object as _;
use object::ObjectSection as _;
use object::ObjectSymbol as _;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const LINUX_TREE_VAR: &str = "WILD_LINUX_TREE";
const TEST_NAME: &str = "elf/x86_64/vmlinux";

const SCRIPT: &str = "arch/x86/kernel/vmlinux.lds";
const GNU_ORACLE: &str = "vmlinux.unstripped";
const WHOLE_ARCHIVE: &str = "vmlinux.o";
const EXTRA_OBJECTS: &[&str] = &[
    ".vmlinux.export.o",
    "init/version-timestamp.o",
    ".tmp_vmlinux2.kallsyms.o",
];

/// Layout-sensitive symbols that have been matched against GNU ld.
const KEY_SYMBOLS: &[&str] = &[
    "_stext",
    "_etext",
    "__init_begin",
    "_end",
    "__sched_class_highest",
    "startup_64",
    "phys_startup_64",
    "text_size",
    "jiffies",
    "jiffies_64",
    "current_task",
    "const_current_task",
    "cpu_current_top_of_stack",
    "const_cpu_current_top_of_stack",
    "__stack_chk_guard",
    "__ref_stack_chk_guard",
    "__preempt_count",
    "hardirq_stack_ptr",
];

pub(super) fn collect_tests(tests: &mut Vec<Trial>, filter: &Filter) {
    if filter.excludes(TEST_NAME) {
        return;
    }
    tests.push(Trial::ignorable_test(TEST_NAME, || {
        run_vmlinux_test().map_err(|e| libtest_mimic::Failed::from(e.to_string()))
    }));
}

fn run_vmlinux_test() -> Result<libtest_mimic::Completion> {
    let Some(tree) = std::env::var_os(LINUX_TREE_VAR).map(PathBuf::from) else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{LINUX_TREE_VAR} is unset"
        )));
    };

    let script = tree.join(SCRIPT);
    let gnu = tree.join(GNU_ORACLE);
    let vmlinux_o = tree.join(WHOLE_ARCHIVE);
    let extras: Vec<PathBuf> = EXTRA_OBJECTS.iter().map(|p| tree.join(p)).collect();

    let mut missing = Vec::new();
    for path in std::iter::once(&script)
        .chain(std::iter::once(&gnu))
        .chain(std::iter::once(&vmlinux_o))
        .chain(extras.iter())
    {
        if !path.is_file() {
            missing.push(path.display().to_string());
        }
    }
    if !missing.is_empty() {
        bail!(
            "{LINUX_TREE_VAR} is set to `{}` but missing: {}",
            tree.display(),
            missing.join(", ")
        );
    }

    let out_dir = build_dir().join("elf/x86_64/vmlinux");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let out = out_dir.join("vmlinux.wild");

    let status = Command::new(wild_path())
        .current_dir(&tree)
        .args([
            "-m",
            "elf_x86_64",
            "-z",
            "noexecstack",
            "--no-warn-rwx-segments",
            "-z",
            "max-page-size=0x200000",
            "--build-id=sha1",
            "--orphan-handling=error",
            "--emit-relocs",
            "--discard-none",
            "--no-gc-sections",
            "--no-identity-comment",
        ])
        .arg(format!("--script={}", script.display()))
        .arg("-o")
        .arg(&out)
        .arg("--whole-archive")
        .arg(WHOLE_ARCHIVE)
        .arg("--no-whole-archive")
        .args(EXTRA_OBJECTS)
        .status()
        .with_context(|| format!("Failed to spawn {}", wild_path().display()))?;
    if !status.success() {
        bail!("Wild failed to link vmlinux ({status})");
    }

    compare_key_symbols(&gnu, &out)
        .with_context(|| format!("Wild `{}` vs GNU `{}`", out.display(), gnu.display()))?;
    Ok(libtest_mimic::Completion::Completed)
}

#[derive(Debug, PartialEq, Eq)]
struct SymInfo {
    address: u64,
    section: String,
}

fn compare_key_symbols(gnu: &Path, wild: &Path) -> Result {
    let gnu_syms = load_key_symbols(gnu)?;
    let wild_syms = load_key_symbols(wild)?;
    let mut mismatches = Vec::new();
    for name in KEY_SYMBOLS {
        match (gnu_syms.get(*name), wild_syms.get(*name)) {
            (None, None) => mismatches.push(format!("{name}: missing in GNU and Wild")),
            (None, Some(_)) => mismatches.push(format!("{name}: missing in GNU")),
            (Some(_), None) => mismatches.push(format!("{name}: missing in Wild")),
            (Some(g), Some(w)) if g != w => {
                mismatches.push(format!(
                    "{name}: GNU {} @ {:#x} vs Wild {} @ {:#x}",
                    g.section, g.address, w.section, w.address
                ));
            }
            (Some(_), Some(_)) => {}
        }
    }
    if !mismatches.is_empty() {
        bail!("{}", mismatches.join("\n"));
    }
    Ok(())
}

fn load_key_symbols(path: &Path) -> Result<HashMap<String, SymInfo>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let obj = object::File::parse(bytes.as_slice())
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let wanted: HashSet<&str> = KEY_SYMBOLS.iter().copied().collect();
    let mut out = HashMap::new();
    for sym in obj.symbols() {
        let Ok(name) = sym.name() else {
            continue;
        };
        if !wanted.contains(name) {
            continue;
        }
        let section = match sym.section() {
            object::SymbolSection::Absolute => "ABS".to_owned(),
            object::SymbolSection::Section(index) => obj
                .section_by_index(index)
                .ok()
                .and_then(|s| s.name().ok().map(str::to_owned))
                .unwrap_or_else(|| format!("section#{}", index.0)),
            other => format!("{other:?}"),
        };
        out.insert(
            name.to_owned(),
            SymInfo {
                address: sym.address(),
                section,
            },
        );
    }
    Ok(out)
}
