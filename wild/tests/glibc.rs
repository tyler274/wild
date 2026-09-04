//! Opt-in x86_64 glibc DSO relink against a GNU-built tree (`ld.so`, `libc.so`,
//! then GNU `lib%.so` PIC archives: `libm`, `libresolv`, `libmvec`, `libnsl`,
//! `libthread_db`, `libc_malloc_debug`, `libnss_{compat,db,hesiod}`,
//! `libBrokenLocale`, `libmemusage`, `libpcprofile`, and the
//! `libpthread` / `libdl` / `librt` / `libutil` / `libanl` stubs).
//!
//! Glibc's `configure` accepts GNU ld, gold, or LLD version strings. Wild's
//! `--version` first line is GNU ld compatible, but the GNU oracle is still
//! compiled with GNU ld so the relink tests have a BFD binary to diff against.
//! `nix develop` sets `WILD_GLIBC_TREE` / `WILD_GLIBC_BUILD` and provides
//! `wild-build-glibc`. Otherwise set `WILD_GLIBC_TREE` to a source checkout and
//! `WILD_GLIBC_BUILD` to the out-of-tree build (default: `<tree>/../glibc-build`).
//!
//! Skipped when `WILD_GLIBC_TREE` is unset, or when the build does not yet
//! contain the expected objects (a from-scratch glibc build will not fit the
//! 10-minute CI timeout). GNU ld is the only oracle.

use crate::Filter;
use crate::build_dir;
use crate::incremental_check;
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
const LDSO_INCR_TEST: &str = "elf/x86_64/glibc-ldso-incremental";
const LIBC_TEST: &str = "elf/x86_64/glibc-libc";
const LIBC_INCR_TEST: &str = "elf/x86_64/glibc-libc-incremental";
const LIBM_INCR_TEST: &str = "elf/x86_64/glibc-libm-incremental";

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

/// GNU ld emits this weak undef in every `gcc -shared` link. It is not a glibc
/// export and Wild does not synthesise it.
const GNU_SYNTHETIC_DYNSYMS: &[&str] = &["__gmon_start__"];

#[derive(Clone, Copy)]
struct PicShlib {
    test_name: &'static str,
    gnu: &'static str,
    pic: &'static str,
    map: &'static str,
    soname: &'static str,
    named_dynsyms: &'static [&'static str],
    extra_needed: &'static [&'static str],
    smoke: Option<&'static str>,
    /// GNU `libfoo.so-no-z-defs = yes` (e.g. libthread_db's debugger `ps_*`).
    no_z_defs: bool,
    /// Glibc `libc-for-link`. `None` uses `libc.so`. libnsl and NSS DSOs that
    /// still call deprecated RPC APIs use `linkobj/libc.so`, where those
    /// symbols are default versions (`xdr_array@@GLIBC_2.2.5`). The installed
    /// `libc.so` keeps them hidden (`xdr_array@GLIBC_2.2.5`), which GNU ld
    /// will not bind to an unversioned reference under `-z defs`.
    libc_for_link: Option<&'static str>,
}

/// GNU `lib%.so: lib%_pic.a` shared objects (not libc / ld.so, which use
/// `-nostdlib` and a different input order).
const PIC_SHLIBS: &[PicShlib] = &[
    PicShlib {
        test_name: "elf/x86_64/glibc-libm",
        gnu: "math/libm.so",
        pic: "math/libm_pic.a",
        map: "libm.map",
        soname: "libm.so.6",
        named_dynsyms: &["sin", "cos", "sqrt", "pow", "nan"],
        extra_needed: &[],
        smoke: Some("math/basic-test"),
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libresolv",
        gnu: "resolv/libresolv.so",
        pic: "resolv/libresolv_pic.a",
        map: "libresolv.map",
        soname: "libresolv.so.2",
        named_dynsyms: &["inet_net_pton", "ns_initparse"],
        extra_needed: &[],
        smoke: Some("resolv/tst-aton"),
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libmvec",
        gnu: "mathvec/libmvec.so",
        pic: "mathvec/libmvec_pic.a",
        map: "libmvec.map",
        soname: "libmvec.so.1",
        named_dynsyms: &["_ZGVcN4v_exp", "_ZGVcN4v_log"],
        extra_needed: &["math/libm.so"],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libpthread",
        gnu: "nptl/libpthread.so",
        pic: "nptl/libpthread_pic.a",
        map: "libpthread.map",
        soname: "libpthread.so.0",
        named_dynsyms: &["__libpthread_version_placeholder"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libdl",
        gnu: "dlfcn/libdl.so",
        pic: "dlfcn/libdl_pic.a",
        map: "libdl.map",
        soname: "libdl.so.2",
        named_dynsyms: &["__libdl_version_placeholder"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-librt",
        gnu: "rt/librt.so",
        pic: "rt/librt_pic.a",
        map: "librt.map",
        soname: "librt.so.1",
        named_dynsyms: &["__librt_version_placeholder"],
        extra_needed: &[],
        smoke: Some("rt/tst-timer"),
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libnsl",
        gnu: "nis/libnsl.so",
        pic: "nis/libnsl_pic.a",
        map: "libnsl.map",
        soname: "libnsl.so.1",
        named_dynsyms: &["yp_all", "nis_leaf_of"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: Some("linkobj/libc.so"),
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libthread_db",
        gnu: "nptl_db/libthread_db.so",
        pic: "nptl_db/libthread_db_pic.a",
        map: "libthread_db.map",
        soname: "libthread_db.so.1",
        named_dynsyms: &["td_ta_get_nthreads", "td_thr_get_info"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: true,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libc_malloc_debug",
        gnu: "malloc/libc_malloc_debug.so",
        pic: "malloc/libc_malloc_debug_pic.a",
        map: "libc_malloc_debug.map",
        soname: "libc_malloc_debug.so.0",
        named_dynsyms: &["malloc", "free", "mallopt"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libnss_compat",
        gnu: "nss/libnss_compat.so",
        pic: "nss/libnss_compat_pic.a",
        map: "libnss_compat.map",
        soname: "libnss_compat.so.2",
        named_dynsyms: &["_nss_compat_getpwnam_r", "_nss_compat_getgrnam_r"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: Some("linkobj/libc.so"),
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libnss_db",
        gnu: "nss/libnss_db.so",
        pic: "nss/libnss_db_pic.a",
        map: "libnss_db.map",
        soname: "libnss_db.so.2",
        named_dynsyms: &["_nss_db_getpwnam_r", "_nss_db_getgrnam_r"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: Some("linkobj/libc.so"),
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libnss_hesiod",
        gnu: "hesiod/libnss_hesiod.so",
        pic: "hesiod/libnss_hesiod_pic.a",
        map: "libnss_hesiod.map",
        soname: "libnss_hesiod.so.2",
        named_dynsyms: &["_nss_hesiod_getpwnam_r", "_nss_hesiod_getgrnam_r"],
        extra_needed: &["resolv/libresolv.so", "nss/libnss_files.so"],
        smoke: None,
        no_z_defs: false,
        libc_for_link: Some("linkobj/libc.so"),
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libBrokenLocale",
        gnu: "locale/libBrokenLocale.so",
        pic: "locale/libBrokenLocale_pic.a",
        map: "libBrokenLocale.map",
        soname: "libBrokenLocale.so.1",
        named_dynsyms: &["__ctype_get_mb_cur_max"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libutil",
        gnu: "login/libutil.so",
        pic: "login/libutil_pic.a",
        map: "libutil.map",
        soname: "libutil.so.1",
        named_dynsyms: &["__libutil_version_placeholder"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libanl",
        gnu: "resolv/libanl.so",
        pic: "resolv/libanl_pic.a",
        map: "libanl.map",
        soname: "libanl.so.1",
        named_dynsyms: &["__libanl_version_placeholder"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libmemusage",
        gnu: "malloc/libmemusage.so",
        pic: "malloc/libmemusage_pic.a",
        map: "libmemusage.map",
        soname: "libmemusage.so",
        named_dynsyms: &["malloc", "free", "calloc", "realloc"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
    PicShlib {
        test_name: "elf/x86_64/glibc-libpcprofile",
        gnu: "debug/libpcprofile.so",
        pic: "debug/libpcprofile_pic.a",
        map: "libpcprofile.map",
        soname: "libpcprofile.so",
        named_dynsyms: &["__cyg_profile_func_enter", "__cyg_profile_func_exit"],
        extra_needed: &[],
        smoke: None,
        no_z_defs: false,
        libc_for_link: None,
    },
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
    if !filter.excludes(LDSO_INCR_TEST) {
        tests.push(Trial::ignorable_test(LDSO_INCR_TEST, || {
            run_ldso_incremental_test().map_err(|e| libtest_mimic::Failed::from(e.to_string()))
        }));
    }
    if !filter.excludes(LIBC_INCR_TEST) {
        tests.push(Trial::ignorable_test(LIBC_INCR_TEST, || {
            run_libc_incremental_test().map_err(|e| libtest_mimic::Failed::from(e.to_string()))
        }));
    }
    if !filter.excludes(LIBM_INCR_TEST) {
        tests.push(Trial::ignorable_test(LIBM_INCR_TEST, || {
            run_libm_incremental_test().map_err(|e| libtest_mimic::Failed::from(e.to_string()))
        }));
    }
    for spec in PIC_SHLIBS {
        if filter.excludes(spec.test_name) {
            continue;
        }
        tests.push(Trial::ignorable_test(spec.test_name, move || {
            run_pic_shlib_test(spec).map_err(|e| libtest_mimic::Failed::from(e.to_string()))
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

fn ldso_command(out: &Path, librtld: &Path, map: Option<&Path>) -> Command {
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
    .arg(out)
    .arg(librtld);
    if let Some(map) = map {
        cmd.arg(format!("--version-script={}", map.display()));
    }
    cmd
}

fn libc_command(
    out: &Path,
    pic: &Path,
    abi_note: Option<&Path>,
    interp: Option<&Path>,
    ldso: Option<&Path>,
    sofini: Option<&Path>,
    map: Option<&Path>,
    libgcc: &Path,
) -> Command {
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
    .arg(out);
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
    cmd
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

    let mut cmd = ldso_command(&out, &librtld, map.as_deref());
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

    let mut cmd = libc_command(
        &out,
        &pic,
        abi_note.as_deref(),
        interp.as_deref(),
        ldso.as_deref(),
        sofini.as_deref(),
        map.as_deref(),
        &libgcc,
    );
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

fn with_incremental(mut cmd: Command) -> Command {
    cmd.arg("--incremental");
    cmd
}

fn run_ldso_incremental_test() -> Result<libtest_mimic::Completion> {
    let Some((_tree, build)) = glibc_paths()? else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{TREE_VAR} is unset"
        )));
    };

    let librtld = build.join("elf/librtld.os");
    let map = first_existing(&build, &["elf/ld.map", "ld.map"]);
    if !librtld.is_file() {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no elf/librtld.os yet (configure && make)",
            build.display()
        )));
    }

    let out_dir = build_dir().join(LDSO_INCR_TEST);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let out = out_dir.join("ld.so.incr.wild");
    incremental_check::relink_unchanged(
        with_incremental(ldso_command(&out, &librtld, map.as_deref())),
        with_incremental(ldso_command(&out, &librtld, map.as_deref())),
        &out,
        true,
        false,
    )?;
    Ok(libtest_mimic::Completion::Completed)
}

fn run_libc_incremental_test() -> Result<libtest_mimic::Completion> {
    let Some((_tree, build)) = glibc_paths()? else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{TREE_VAR} is unset"
        )));
    };

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

    let out_dir = build_dir().join(LIBC_INCR_TEST);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let out = out_dir.join("libc.so.incr.wild");
    let libgcc = libgcc_archive()?;
    let command = || {
        libc_command(
            &out,
            &pic,
            abi_note.as_deref(),
            interp.as_deref(),
            ldso.as_deref(),
            sofini.as_deref(),
            map.as_deref(),
            &libgcc,
        )
    };
    incremental_check::relink_unchanged(
        with_incremental(command()),
        with_incremental(command()),
        &out,
        true,
        true,
    )?;
    Ok(libtest_mimic::Completion::Completed)
}

fn run_libm_incremental_test() -> Result<libtest_mimic::Completion> {
    let Some((_tree, build)) = glibc_paths()? else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{TREE_VAR} is unset"
        )));
    };
    let spec = PIC_SHLIBS
        .iter()
        .find(|s| s.test_name == "elf/x86_64/glibc-libm")
        .expect("PIC_SHLIBS includes glibc-libm");
    let pic = build.join(spec.pic);
    if !pic.is_file() {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no {} yet (configure && make)",
            build.display(),
            spec.pic
        )));
    }
    let Some(libc) = first_existing(&build, &["libc.so"]) else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no libc.so yet (configure && make)",
            build.display()
        )));
    };

    let out_dir = build_dir().join(LIBM_INCR_TEST);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let out = out_dir.join("libm.so.incr.wild");
    let command = || -> Result<Command> { glibc_pic_command(spec, &out, &build, &pic, &libc) };
    incremental_check::relink_unchanged(
        with_incremental(command()?),
        with_incremental(command()?),
        &out,
        true,
        true,
    )?;
    Ok(libtest_mimic::Completion::Completed)
}

fn glibc_pic_command(
    spec: &PicShlib,
    out: &Path,
    build: &Path,
    pic: &Path,
    libc: &Path,
) -> Result<Command> {
    let abi_note = first_existing(build, &["csu/abi-note.o"]);
    let libc_nonshared = first_existing(build, &["libc_nonshared.a"]);
    let ldso = first_existing(build, &["elf/ld.so"]);
    let map = first_existing(build, &[spec.map]);
    let libgcc = libgcc_archive()?;
    let crtbegin = gcc_print_file_name("crtbeginS.o")?;
    let crtend = gcc_print_file_name("crtendS.o")?;

    let mut cmd = Command::new(wild_path());
    cmd.args(["-shared"]);
    if !spec.no_z_defs {
        cmd.args(["-z", "defs"]);
    }
    cmd.args([
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
        spec.soname,
        "-o",
    ])
    .arg(out);
    if let Some(map) = map {
        cmd.arg(format!("--version-script={}", map.display()));
    }
    if let Some(abi_note) = abi_note {
        cmd.arg(abi_note);
    }
    cmd.arg(&crtbegin)
        .arg("--whole-archive")
        .arg(pic)
        .arg("--no-whole-archive");
    for extra in spec.extra_needed {
        cmd.arg(build.join(extra));
    }
    cmd.arg("--start-group").arg(libc);
    if let Some(libc_nonshared) = libc_nonshared {
        cmd.arg(libc_nonshared);
    }
    cmd.arg("--as-needed");
    if let Some(ldso) = ldso {
        cmd.arg(ldso);
    }
    cmd.arg("--no-as-needed")
        .arg("--end-group")
        .arg(&libgcc)
        .arg(&crtend);
    Ok(cmd)
}

fn run_pic_shlib_test(spec: &PicShlib) -> Result<libtest_mimic::Completion> {
    let Some((_tree, build)) = glibc_paths()? else {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{TREE_VAR} is unset"
        )));
    };

    let gnu = build.join(spec.gnu);
    let pic = build.join(spec.pic);
    let abi_note = first_existing(&build, &["csu/abi-note.o"]);
    let libc = first_existing(&build, &["libc.so"]);
    let libc_nonshared = first_existing(&build, &["libc_nonshared.a"]);
    let ldso = first_existing(&build, &["elf/ld.so"]);
    let map = first_existing(&build, &[spec.map]);
    let libc = match spec.libc_for_link {
        Some(rel) => first_existing(&build, &[rel]),
        None => libc,
    };

    if !pic.is_file() {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no {} yet (configure && make)",
            build.display(),
            spec.pic
        )));
    }
    if !gnu.is_file() {
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no {} yet (configure && make)",
            build.display(),
            spec.gnu
        )));
    }
    let Some(libc) = libc else {
        let missing = spec.libc_for_link.unwrap_or("libc.so");
        return Ok(libtest_mimic::Completion::ignored_with(format!(
            "{} has no {missing} yet (configure && make)",
            build.display()
        )));
    };

    let out_dir = build_dir().join(spec.test_name);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create {}", out_dir.display()))?;
    let file_name = Path::new(spec.gnu)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lib.so");
    let out = out_dir.join(format!("{file_name}.wild"));

    let libgcc = libgcc_archive()?;
    let crtbegin = gcc_print_file_name("crtbeginS.o")?;
    let crtend = gcc_print_file_name("crtendS.o")?;

    let mut cmd = Command::new(wild_path());
    cmd.args(["-shared"]);
    if !spec.no_z_defs {
        cmd.args(["-z", "defs"]);
    }
    cmd.args([
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
        spec.soname,
        "-o",
    ])
    .arg(&out);
    if let Some(map) = map {
        cmd.arg(format!("--version-script={}", map.display()));
    }
    // GNU `lib%.so` from `lib%_pic.a`: gcc -shared inserts crtbeginS/crtendS
    // (libc uses -nostdlib, these DSOs do not).
    if let Some(abi_note) = abi_note {
        cmd.arg(abi_note);
    }
    cmd.arg(&crtbegin)
        .arg("--whole-archive")
        .arg(&pic)
        .arg("--no-whole-archive");
    for extra in spec.extra_needed {
        let path = build.join(extra);
        if !path.is_file() {
            return Ok(libtest_mimic::Completion::ignored_with(format!(
                "{} has no {extra} yet (configure && make)",
                build.display()
            )));
        }
        cmd.arg(path);
    }
    cmd.arg("--start-group").arg(&libc);
    if let Some(libc_nonshared) = libc_nonshared {
        cmd.arg(libc_nonshared);
    }
    cmd.arg("--as-needed");
    if let Some(ldso) = ldso {
        cmd.arg(ldso);
    }
    cmd.arg("--no-as-needed")
        .arg("--end-group")
        .arg(&libgcc)
        .arg(&crtend);
    let status = cmd
        .status()
        .with_context(|| format!("Failed to spawn {}", wild_path().display()))?;
    if !status.success() {
        bail!("Wild failed to link {} ({status})", spec.gnu);
    }

    check_soname(&out, spec.soname)?;
    if !spec.named_dynsyms.is_empty() {
        check_named_dynsyms(&out, spec.named_dynsyms)?;
    }
    compare_dynsym_names(&gnu, &out)?;

    let libdir = out_dir.join("lib");
    std::fs::create_dir_all(&libdir)
        .with_context(|| format!("Failed to create {}", libdir.display()))?;
    let staged = libdir.join(spec.soname);
    std::fs::copy(&out, &staged)
        .with_context(|| format!("Failed to stage {} as {}", out.display(), staged.display()))?;
    let gnu_ldso = build.join("elf/ld.so");
    if let Some(smoke) = spec.smoke {
        smoke_run(
            &gnu_ldso,
            &glibc_library_path(&build, Some(&libdir)),
            &build.join(smoke),
        )?;
    }
    Ok(libtest_mimic::Completion::Completed)
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

fn libgcc_archive() -> Result<PathBuf> {
    gcc_cc_print("-print-libgcc-file-name")
}

fn gcc_print_file_name(name: &str) -> Result<PathBuf> {
    gcc_cc_print(&format!("-print-file-name={name}"))
}

fn gcc_cc_print(flag: &str) -> Result<PathBuf> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".to_owned());
    let output = Command::new(&cc)
        .arg(flag)
        .output()
        .with_context(|| format!("Failed to run `{cc} {flag}`"))?;
    if !output.status.success() {
        bail!("`{cc} {flag}` failed ({})", output.status);
    }
    let path = String::from_utf8(output.stdout)
        .context("gcc print-file-name output is not UTF-8")?
        .trim()
        .to_owned();
    if path.is_empty() || path == flag.rsplit_once('=').map(|(_, n)| n).unwrap_or("") {
        bail!("`{cc} {flag}` printed nothing");
    }
    let path = PathBuf::from(path);
    if !path.is_file() {
        bail!("`{cc} {flag}` is not a file (`{}`)", path.display());
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
    // Defined exports only. Wild GCs unused objects inside `--whole-archive`
    // PIC stubs (e.g. librt), so GNU leftover UND imports are not required.
    let gnu_names = dynsym_defined_names(gnu)?;
    let wild_names = dynsym_defined_names(wild)?;
    let mut missing: Vec<String> = gnu_names
        .difference(&wild_names)
        .filter(|n| !n.is_empty() && !GNU_SYNTHETIC_DYNSYMS.contains(&n.as_str()))
        .cloned()
        .collect();
    missing.sort_unstable();
    if missing.len() > 20 {
        missing.truncate(20);
        bail!(
            "Wild {} missing GNU exported dynamic symbols (first 20): {}",
            wild.display(),
            missing.join(", ")
        );
    }
    if !missing.is_empty() {
        bail!(
            "Wild {} missing GNU exported dynamic symbols: {}",
            wild.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn dynsym_defined_names(path: &Path) -> Result<HashSet<String>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let obj = object::File::parse(bytes.as_slice())
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(obj
        .dynamic_symbols()
        .filter(|s| !s.is_undefined())
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
    smoke_run(ldso, library_path, &build.join("io/pwd"))
}

fn smoke_run(ldso: &Path, library_path: &OsString, exe: &Path) -> Result {
    if !exe.is_file() {
        return Ok(());
    }
    let output = Command::new(ldso)
        .arg("--library-path")
        .arg(library_path)
        .arg(exe)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("Failed to spawn {}", ldso.display()))?;
    if !output.status.success() {
        bail!(
            "{} failed to run {} ({}): {}",
            ldso.display(),
            exe.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}
