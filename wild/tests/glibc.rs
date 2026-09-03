//! Opt-in x86_64 glibc `ld.so` / `libc.so` relink against a GNU-built tree.
//!
//! Glibc's `configure` accepts GNU ld, gold, or LLD version strings. Wild's
//! `--version` first line is GNU ld compatible, but the GNU oracle is still
//! compiled with GNU ld so the relink tests have a BFD binary to diff against.
//! `nix develop` sets `WILD_GLIBC_TREE` / `WILD_GLIBC_BUILD` and provides
//! `wild-build-glibc`. Otherwise set `WILD_GLIBC_TREE` to a source checkout and
//! `WILD_GLIBC_BUILD` to the out-of-tree build (default: `<tree>/../glibc-build`).
//!
//! Skipped when `WILD_GLIBC_TREE` is unset, or when the build does not yet
//! contain `elf/ld.so` / `libc.so` (a from-scratch glibc build will not fit
//! the 10-minute CI timeout). GNU ld is the only oracle.

use crate::Filter;
use crate::build_dir;
use crate::wild_path;
use libtest_mimic::Trial;
use libwild::bail;
use libwild::error::Context as _;
use libwild::error::Result;
use object::Object as _;
use object::ObjectSymbol as _;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const TREE_VAR: &str = "WILD_GLIBC_TREE";
const BUILD_VAR: &str = "WILD_GLIBC_BUILD";
const LDSO_TEST: &str = "elf/x86_64/glibc-ldso";
const LIBC_TEST: &str = "elf/x86_64/glibc-libc";

const LDSO_SONAME: &str = "ld-linux-x86-64.so.2";
const LIBC_SONAME: &str = "libc.so.6";

const LIBC_DYNSYMS: &[&str] = &[
    "malloc",
    "free",
    "memcpy",
    "printf",
    "__libc_start_main",
    "__libc_early_init",
];

pub(super) fn collect_tests(tests: &mut Vec<Trial>, filter: &Filter) {
    if !filter.excludes(LDSO_TEST) {
        tests.push(Trial::ignorable_test(LDSO_TEST, || {
            run_ldso_test().map_err(|e| libtest_mimic::Failed::from(e.to_string()))
        }));
    }
    if !filter.excludes(LIBC_TEST) {
        tests.push(Trial::ignorable_test(LIBC_TEST, || {
            run_libc_test().map_err(|e| libtest_mimic::Failed::from(e.to_string()))
        }));
    }
}

fn glibc_paths() -> Result<Option<(PathBuf, PathBuf)>> {
    let Some(tree) = std::env::var_os(TREE_VAR).map(PathBuf::from) else {
        return Ok(None);
    };
    if !tree.join("Makerules").is_file() || !tree.join("elf/Versions").is_file() {
        bail!(
            "{TREE_VAR} is set to `{}` but that is not a glibc source tree",
            tree.display()
        );
    }
    let build = std::env::var_os(BUILD_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| tree.parent().unwrap_or(tree.as_path()).join("glibc-build"));
    Ok(Some((tree, build)))
}

fn run_ldso_test() -> Result<libtest_mimic::Completion> {
    let Some((_tree, build)) = glibc_paths()? else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{TREE_VAR} is unset"
        )));
    };

    let gnu = build.join("elf/ld.so");
    let librtld = build.join("elf/librtld.os");
    let map = first_existing(&build, &["elf/ld.map", "ld.map"]);
    if !gnu.is_file() || !librtld.is_file() {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no elf/ld.so + elf/librtld.os yet (configure && make)",
            build.display()
        )));
    }

    let out_dir = build_dir().join("elf/x86_64/glibc-ldso");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let out = out_dir.join("ld.so.wild");

    let mut cmd = Command::new(wild_path());
    cmd.args([
        "-shared",
        "-z",
        "defs",
        "-z",
        "relro",
        "-z",
        "now",
        "-z",
        "pack-relative-relocs",
        "-z",
        "nomark-plt",
        "--hash-style=both",
        "-soname",
        LDSO_SONAME,
        "-o",
    ])
    .arg(&out)
    .arg(&librtld);
    if let Some(map) = map {
        cmd.arg(format!("--version-script={}", map.display()));
    }
    let status = cmd
        .status()
        .with_context(|| format!("Failed to spawn {}", wild_path().display()))?;
    if !status.success() {
        bail!("Wild failed to link ld.so ({status})");
    }

    check_no_undefined_dynsyms(&out)?;
    check_soname(&out, LDSO_SONAME)?;
    compare_dynsym_names(&gnu, &out)?;
    smoke_run_pwd(&out, &glibc_library_path(&build, None), &build)?;
    Ok(libtest_mimic::Completion::Completed)
}

fn run_libc_test() -> Result<libtest_mimic::Completion> {
    let Some((_tree, build)) = glibc_paths()? else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{TREE_VAR} is unset"
        )));
    };

    let gnu = build.join("libc.so");
    let pic = first_existing(&build, &["libc_pic.os.clean", "libc_pic.os"]);
    let abi_note = first_existing(&build, &["csu/abi-note.o"]);
    let sofini = first_existing(&build, &["elf/sofini.os"]);
    let interp = first_existing(&build, &["elf/interp.os"]);
    let ldso = first_existing(&build, &["elf/ld.so"]);
    let map = first_existing(&build, &["libc.map"]);

    let Some(pic) = pic else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no libc_pic.os yet (configure && make)",
            build.display()
        )));
    };
    if !gnu.is_file() {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no libc.so yet (configure && make)",
            build.display()
        )));
    }

    let out_dir = build_dir().join("elf/x86_64/glibc-libc");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let out = out_dir.join("libc.so.wild");

    let libgcc = libgcc_archive()?;

    let mut cmd = Command::new(wild_path());
    cmd.args([
        "-shared",
        "-z",
        "defs",
        "-z",
        "relro",
        "-z",
        "now",
        "-z",
        "pack-relative-relocs",
        "-z",
        "nomark-plt",
        "--hash-style=both",
        "-e",
        "__libc_main",
        "-soname",
        LIBC_SONAME,
        "-o",
    ])
    .arg(&out);
    if let Some(map) = map {
        cmd.arg(format!("--version-script={}", map.display()));
    }
    // GNU `build-shlib` order: abi-note, pic, interp, ld.so, libgcc, sofini last.
    for input in [abi_note, Some(pic), interp, ldso, Some(libgcc), sofini]
        .into_iter()
        .flatten()
    {
        cmd.arg(input);
    }
    let status = cmd
        .status()
        .with_context(|| format!("Failed to spawn {}", wild_path().display()))?;
    if !status.success() {
        bail!("Wild failed to link libc.so ({status})");
    }

    check_soname(&out, LIBC_SONAME)?;
    check_named_dynsyms(&out, LIBC_DYNSYMS)?;
    compare_dynsym_names(&gnu, &out)?;

    let libdir = out_dir.join("lib");
    std::fs::create_dir_all(&libdir)
        .with_context(|| format!("Failed to create {}", libdir.display()))?;
    let staged_libc = libdir.join("libc.so.6");
    std::fs::copy(&out, &staged_libc).with_context(|| {
        format!(
            "Failed to stage {} as {}",
            out.display(),
            staged_libc.display()
        )
    })?;
    let gnu_ldso = build.join("elf/ld.so");
    smoke_run_pwd(
        &gnu_ldso,
        &glibc_library_path(&build, Some(&libdir)),
        &build,
    )?;
    Ok(libtest_mimic::Completion::Completed)
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

fn libgcc_archive() -> Result<PathBuf> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".to_owned());
    let output = Command::new(&cc)
        .arg("-print-libgcc-file-name")
        .output()
        .with_context(|| format!("Failed to run `{cc} -print-libgcc-file-name`"))?;
    if !output.status.success() {
        bail!("`{cc} -print-libgcc-file-name` failed ({})", output.status);
    }
    let path = String::from_utf8(output.stdout)
        .context("libgcc path is not UTF-8")?
        .trim()
        .to_owned();
    if path.is_empty() {
        bail!("`{cc} -print-libgcc-file-name` printed nothing");
    }
    let path = PathBuf::from(path);
    if !path.is_file() {
        bail!("libgcc archive `{}` is not a file", path.display());
    }
    Ok(path)
}

fn check_soname(path: &Path, expected: &str) -> Result {
    let output = Command::new("readelf")
        .args(["-d", &path.display().to_string()])
        .output()
        .with_context(|| format!("Failed to run readelf on {}", path.display()))?;
    if !output.status.success() {
        bail!("readelf -d {} failed", path.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let soname = text.lines().find_map(|line| {
        let line = line.trim();
        if !line.contains("(SONAME)") {
            return None;
        }
        line.rsplit_once('[')
            .and_then(|(_, rest)| rest.strip_suffix(']'))
    });
    match soname {
        Some(name) if name == expected => Ok(()),
        Some(name) => bail!(
            "{}: SONAME `{name}` (expected `{expected}`)",
            path.display()
        ),
        None => bail!(
            "{}: missing DT_SONAME (expected `{expected}`)",
            path.display()
        ),
    }
}

fn check_no_undefined_dynsyms(path: &Path) -> Result {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let obj = object::File::parse(bytes.as_slice())
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let mut undefined = Vec::new();
    for sym in obj.dynamic_symbols() {
        if !sym.is_undefined() {
            continue;
        }
        let Ok(name) = sym.name() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        undefined.push(name.to_owned());
    }
    if !undefined.is_empty() {
        bail!(
            "{} has undefined dynamic symbols: {}",
            path.display(),
            undefined.join(", ")
        );
    }
    Ok(())
}

fn check_named_dynsyms(path: &Path, names: &[&str]) -> Result {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let obj = object::File::parse(bytes.as_slice())
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let have: HashSet<&str> = obj
        .dynamic_symbols()
        .filter_map(|s| s.name().ok())
        .collect();
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !have.contains(n))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} missing dynamic symbols: {}",
            path.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn compare_dynsym_names(gnu: &Path, wild: &Path) -> Result {
    let gnu_names = dynsym_names(gnu)?;
    let wild_names = dynsym_names(wild)?;
    let mut missing: Vec<String> = gnu_names
        .difference(&wild_names)
        .filter(|n| !n.is_empty())
        .cloned()
        .collect();
    missing.sort_unstable();
    if missing.len() > 20 {
        missing.truncate(20);
        bail!(
            "Wild {} missing GNU dynamic symbols (first 20): {}",
            wild.display(),
            missing.join(", ")
        );
    }
    if !missing.is_empty() {
        bail!(
            "Wild {} missing GNU dynamic symbols: {}",
            wild.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn dynsym_names(path: &Path) -> Result<HashSet<String>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let obj = object::File::parse(bytes.as_slice())
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(obj
        .dynamic_symbols()
        .filter_map(|s| s.name().ok().map(str::to_owned))
        .filter(|n| !n.is_empty())
        .collect())
}

const GLIBC_LIB_SUBDIRS: &[&str] = &[
    "math", "elf", "dlfcn", "nss", "nis", "rt", "resolv", "mathvec", "support", "misc", "debug",
    "nptl",
];

fn glibc_library_path(build: &Path, prepend: Option<&Path>) -> OsString {
    let mut dirs = Vec::new();
    if let Some(path) = prepend {
        dirs.push(path.to_path_buf());
    }
    dirs.push(build.to_path_buf());
    dirs.extend(GLIBC_LIB_SUBDIRS.iter().map(|sub| build.join(sub)));
    std::env::join_paths(dirs).expect("glibc library path contains interior NULs")
}

fn smoke_run_pwd(ldso: &Path, library_path: &OsString, build: &Path) -> Result {
    let pwd = build.join("io/pwd");
    if !pwd.is_file() {
        return Ok(());
    }
    let output = Command::new(ldso)
        .arg("--library-path")
        .arg(library_path)
        .arg(&pwd)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("Failed to spawn {}", ldso.display()))?;
    if !output.status.success() {
        bail!(
            "{} failed to run {} ({}): {}",
            ldso.display(),
            pwd.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}
