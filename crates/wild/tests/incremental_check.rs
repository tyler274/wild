//! Shared incremental-relink checks for the kernel and glibc opt-in tests.

use libwild::bail;
use libwild::ensure;
use libwild::error::Context as _;
use libwild::error::Result;
use std::path::Path;
use std::path::PathBuf;
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
