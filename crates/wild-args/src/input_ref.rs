use std::fmt::Display;
use std::path::Path;
use wild_fs::archive;
use wild_scripts::Modifiers;
use wild_scripts::linker_script::LinkerScript;

/// Type-erased view of an input file path and modifiers.
#[derive(Debug, Clone, Copy)]
pub struct InputFileRef<'data> {
    pub filename: &'data Path,
    pub original_filename: &'data Path,
    pub modifiers: Modifiers,
}

impl InputFileRef<'_> {
    #[must_use]
    pub fn for_testing() -> Self {
        Self {
            filename: Path::new(""),
            original_filename: Path::new(""),
            modifiers: Modifiers::default(),
        }
    }
}

/// Identifies an input object that may not be a regular file on disk, or may be an entry in an
/// archive.
#[derive(Clone, Copy)]
pub struct InputRef<'data> {
    pub file: InputFileRef<'data>,
    pub data: &'data [u8],
    pub entry: Option<archive::EntryMeta<'data>>,
}

impl Display for InputRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.file.filename.display(), f)?;
        if let Some(entry) = &self.entry {
            Display::fmt(" @ ", f)?;
            Display::fmt(&String::from_utf8_lossy(entry.identifier.as_slice()), f)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for InputRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl<'data> InputRef<'data> {
    #[must_use]
    pub fn lib_name(&self) -> &'data [u8] {
        self.file.original_filename.as_os_str().as_encoded_bytes()
    }

    #[must_use]
    pub fn has_archive_semantics(&self) -> bool {
        self.entry.is_some() || self.file.modifiers.archive_semantics
    }

    #[must_use]
    pub fn data(&self) -> &'data [u8] {
        self.data
    }

    #[must_use]
    pub fn is_archive_entry(&self) -> bool {
        self.entry.is_some()
    }
}

/// A parsed linker script plus the input file it came from.
#[derive(Debug)]
pub struct InputLinkerScript<'data> {
    pub script: LinkerScript<'data>,
    pub input_file: InputFileRef<'data>,
    /// Raw bytes of the script file. Used to compute line numbers from `AssertCommand::remainder`.
    pub script_bytes: &'data [u8],
}
