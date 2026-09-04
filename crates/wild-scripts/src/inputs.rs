use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct Modifiers {
    /// Whether shared objects should only be linked if they're referenced.
    pub as_needed: bool,

    /// Whether we're currently allowed to link against shared libraries.
    pub allow_shared: bool,

    /// Whether object files in archives should be linked even if they do not contain symbols that
    /// are referenced.
    pub whole_archive: bool,

    /// Whether archive semantics should be applied even for regular objects.
    pub archive_semantics: bool,

    /// Whether the file is known to be a temporary file that will be deleted when the linker
    /// exits, e.g. an output file from a linker plugin. This doesn't affect linking, but is
    /// stored in the layout file if written so that linker-diff knows not to error if the file
    /// is missing.
    pub temporary: bool,
}

impl Default for Modifiers {
    fn default() -> Self {
        Self {
            as_needed: false,
            allow_shared: true,
            whole_archive: false,
            archive_semantics: false,
            temporary: false,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Input {
    pub spec: InputSpec,
    /// A directory to search first. Only present when the input came from a linker script, in
    /// which case this is the directory containing the linker script.
    pub search_first: Option<PathBuf>,
    pub modifiers: Modifiers,
}

#[derive(Debug, Eq, PartialEq)]
pub enum InputSpec {
    /// Path (possibly just a filename) to the file.
    File(Box<Path>),
    /// Name of the library, without prefix and suffix.
    Lib(Box<str>),
    /// Name of the library, including prefix and suffix.
    Search(Box<str>),
}
