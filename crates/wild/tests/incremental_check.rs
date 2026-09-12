//! Shared incremental-relink checks for the kernel and glibc opt-in tests.

use libwild::error::{Context as _, Result};
use libwild::{bail, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn state_dir(output: &Path) -> PathBuf {
    let mut dir = output.as_os_str().to_os_string();
    dir.push(".incr");
    PathBuf::from(dir)
}

pub(crate) fn clear_state(output: &Path) {
    let _ = std::fs::remove_file(output);
    let _ = std::fs::remove_dir_all(state_dir(output));
}

pub(crate) fn run_wild(cmd: &mut Command, what: &str) -> Result {
    let status = cmd
        .status()
        .with_context(|| format!("Failed to spawn incremental {what}"))?;
    if !status.success() {
        bail!("Wild incremental {what} failed ({status})");
    }
    Ok(())
}

pub(crate) struct IncrementalLog {
    pub last_line: String,
    pub skip_payloads: u64,
    pub is_update: bool,
    pub is_fallback: bool,
    pub strict_order: bool,
}

pub(crate) fn read_log(output: &Path) -> Result<IncrementalLog> {
    let log_path = state_dir(output).join("log");
    let log = std::fs::read_to_string(&log_path)
        .with_context(|| format!("Failed to read {}", log_path.display()))?;
    let last_line = log.lines().next_back().unwrap_or("").to_owned();
    if last_line.is_empty() {
        bail!("Incremental log {} is empty", log_path.display());
    }
    let skip_payloads = last_line
        .rsplit_once("skip_payloads=")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("Failed to parse skip_payloads from `{last_line}`"))?;
    Ok(IncrementalLog {
        skip_payloads,
        is_update: last_line.contains("incremental-update"),
        is_fallback: last_line.contains("fallback"),
        strict_order: last_line.contains("strict_order=true") || last_line.contains("strict-order"),
        last_line,
    })
}

/// Initial `--incremental` link, then an unchanged second link.
///
/// `require_skips` fails if the update copied every payload. Shared objects with `.init`/`.fini`
/// may do a strict-order full link (`allow_strict_order`).
pub(crate) fn relink_unchanged(
    mut initial: Command,
    mut update: Command,
    output: &Path,
    require_skips: bool,
    allow_strict_order: bool,
) -> Result<IncrementalLog> {
    clear_state(output);
    run_wild(&mut initial, "initial link")?;
    let inputs = state_dir(output).join("inputs.txt");
    ensure!(
        inputs.is_file(),
        "Incremental state {} was not created",
        inputs.display()
    );
    run_wild(&mut update, "unchanged relink")?;
    let log = read_log(output)?;
    if log.is_fallback {
        bail!("Unchanged incremental relink fell back: {}", log.last_line);
    }
    ensure!(
        log.is_update,
        "Expected incremental-update, got: {}",
        log.last_line
    );
    if require_skips && log.skip_payloads == 0 && !(allow_strict_order && log.strict_order) {
        bail!(
            "Unchanged incremental relink skipped no payloads: {}",
            log.last_line
        );
    }
    Ok(log)
}

struct DirtyGuard {
    path: PathBuf,
    original: Vec<u8>,
}

impl Drop for DirtyGuard {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
    }
}

/// Flip one byte past the ELF ident so section sizes stay the same. Restores
/// `path` when dropped. `inputs.txt` mtime is 1s granularity; a byte change is
/// what the skip planner actually notices.
pub(crate) fn dirty_object(path: &Path) -> Result<impl Drop> {
    let original =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    ensure!(
        original.len() > 16,
        "{} is too small to dirty as an ELF object",
        path.display()
    );
    let mut dirty = original.clone();
    let idx = dirty.len() - 1;
    dirty[idx] ^= 0x5a;
    std::fs::write(path, &dirty).with_context(|| format!("Failed to dirty {}", path.display()))?;
    Ok(DirtyGuard {
        path: path.to_path_buf(),
        original,
    })
}

/// Unchanged incremental relink, then a dirty relink of `changed_object`.
///
/// Expects `incremental-update`, fewer skipped payloads than the unchanged
/// pass, and at least one remaining skip (unless `allow_strict_order`).
pub(crate) fn relink_changed(
    initial: Command,
    unchanged: Command,
    mut changed: Command,
    output: &Path,
    changed_object: &Path,
    require_skips: bool,
    allow_strict_order: bool,
) -> Result<IncrementalLog> {
    let before = relink_unchanged(
        initial,
        unchanged,
        output,
        require_skips,
        allow_strict_order,
    )?;
    let _restore = dirty_object(changed_object)?;
    run_wild(&mut changed, "changed relink")?;
    let log = read_log(output)?;
    if log.is_fallback {
        bail!("Changed incremental relink fell back: {}", log.last_line);
    }
    ensure!(
        log.is_update,
        "Expected incremental-update after dirtying {}, got: {}",
        changed_object.display(),
        log.last_line
    );
    if log.skip_payloads >= before.skip_payloads && before.skip_payloads > 0 {
        bail!(
            "Dirtying {} did not reduce skip_payloads ({} -> {}): {}",
            changed_object.display(),
            before.skip_payloads,
            log.skip_payloads,
            log.last_line
        );
    }
    if require_skips && log.skip_payloads == 0 && !(allow_strict_order && log.strict_order) {
        bail!(
            "Changed incremental relink skipped no payloads: {}",
            log.last_line
        );
    }
    Ok(log)
}
