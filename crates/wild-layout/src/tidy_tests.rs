//! Asserts that format-agnostic layout code does not import ELF types.

use std::fs::read_dir;
use std::path::Path;
use wild_error::bail;
use wild_error::error::Result;

/// Patterns that we still allow. These should probably be dealt with, either by renaming these
/// types if we conclude that they're not really ELF-specific, or by removing references to them.
const EXEMPTIONS: &[&str] = &["linker_utils::elf::RelocationKind"];

#[test]
fn check_elf_specific_code() -> Result {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_files(&src_dir)? {
        let contents = std::fs::read_to_string(&path)?;
        let mut skip = false;
        for (i, line) in contents.lines().enumerate() {
            if line.starts_with("#[test]") {
                skip = true;
            } else if line.starts_with('}') {
                skip = false;
            } else if skip {
                continue;
            }

            if line.contains("::elf") && !EXEMPTIONS.iter().any(|e| line.contains(e)) {
                bail!(
                    "{path}:{line} contains ELF-specific code. \
                    Please move code, likely by extending Platform trait",
                    path = path.display(),
                    line = i + 1,
                );
            }
        }
    }
    Ok(())
}

fn rust_files(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    if path.is_dir() {
        let mut files = Vec::new();
        for entry in read_dir(path)? {
            let path = entry?.path();
            if path.is_dir() {
                files.extend(rust_files(&path)?);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
        Ok(files)
    } else {
        Ok(vec![path.to_owned()])
    }
}
