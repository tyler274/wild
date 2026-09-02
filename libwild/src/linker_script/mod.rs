//! This module is responsible for parsing linker scripts.

use std::path::Path;

pub(crate) mod ast;
pub(crate) mod parse;

pub(crate) use ast::*;
pub(crate) use parse::parse_expression;
pub(crate) use parse::skip_comments_and_whitespace;

/// Checks if we need to prefix `input_path` with the sysroot. If we do, then returns the resulting
/// path. Otherwise, returns `None`. `linker_script_path` and `sysroot` should be canonical,
/// absolute paths, otherwise we might not apply the sysroot when we actually should.
pub(crate) fn maybe_apply_sysroot(
    linker_script_path: &Path,
    input_path: &Path,
    sysroot: &Path,
) -> Option<Box<Path>> {
    debug_assert!(linker_script_path.is_absolute());
    debug_assert!(sysroot.is_absolute());
    if linker_script_path.starts_with(sysroot) {
        Some(Box::from(sysroot.join(input_path.strip_prefix("/").ok()?)))
    } else {
        maybe_forced_sysroot(input_path, sysroot)
    }
}

pub(crate) fn maybe_forced_sysroot(lib_path: &Path, sysroot: &Path) -> Option<Box<Path>> {
    let lib_path_str = lib_path.to_string_lossy();
    lib_path_str
        .strip_prefix('=')
        .or_else(|| lib_path_str.strip_prefix("$SYSROOT"))
        .map(|stripped| Box::from(sysroot.join(stripped.trim_start_matches('/'))))
}
