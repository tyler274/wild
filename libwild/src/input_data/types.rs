use crate::FileSystem;
use crate::InputFileData;
use crate::archive;
use crate::args::Modifiers;
use crate::error::Result;
use crate::file_kind::FileKind;
use crate::linker_plugins::LtoInputInfo;
use crate::linker_script::LinkerScript;
use crate::macho_stub_library::DefinedStubLibrary;
use crate::parsing::ParsedInputObject;
use crate::platform::Platform;
use colosseum::sync::Arena;
use std::fmt::Display;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) struct FileLoader<'data, F: FileSystem> {
    /// The files that we've loaded so far.
    pub(crate) loaded_files: Vec<&'data InputFile<F::Input>>,

    /// Whether we have at least one input file that is a dynamic object.
    pub(crate) has_dynamic: bool,

    pub(crate) inputs_arena: &'data Arena<InputFile<F::Input>>,

    // File system used for reading and writing of the data.
    pub(crate) file_system: Arc<F>,
}

#[derive(Default)]
pub(crate) struct LoadedInputs<'data, P: Platform> {
    /// The results of parsing all the input files and archive entries. We defer checking for
    /// success until later, since otherwise a parse error would mean that the save-dir mechanism
    /// wouldn't capture all the input files.
    pub(crate) objects: Vec<Result<Box<ParsedInputObject<'data, P>>>>,

    pub(crate) linker_scripts: Vec<InputLinkerScript<'data>>,

    pub(crate) stub_libraries: Vec<LoadedStubLibrary<'data>>,

    pub(crate) lto_objects: Vec<Result<Box<LtoInputInfo<'data>>>>,

    /// Number of regular objects seen on the command line before the first LTO input. Used to
    /// place plugin codegen at that position (#1935).
    pub(crate) objects_before_first_lto: Option<usize>,
}

pub(crate) struct LoadedStubLibrary<'data> {
    pub(crate) input: InputRef<'data>,
    pub(crate) defined_symbols: DefinedStubLibrary<'data>,
}

pub(crate) struct InputBytes<'data> {
    pub(crate) input: InputRef<'data>,
    pub(crate) kind: FileKind,
    pub(crate) data: &'data [u8],
    pub(crate) modifiers: Modifiers,
}

#[derive(Clone, Copy)]
pub(crate) struct ScriptData<'data> {
    pub(crate) raw: &'data [u8],
}

/// Identifies an input file. IDs start from 0 which is reserved for our prelude file.
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[debug("file-{_0}")]
pub(crate) struct FileId(u32);

pub(crate) const PRELUDE_FILE_ID: FileId = FileId::new(0, 0);

#[derive(Debug)]
pub(crate) struct InputFile<D: InputFileData> {
    pub(crate) filename: PathBuf,

    /// The filename prior to path search. If this is absolute, then `filename` will be the same.
    pub(crate) original_filename: PathBuf,

    pub(crate) modifiers: Modifiers,

    pub(crate) data: Option<D>,
}

// A type used for Type-erasure reasons.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InputFileRef<'data> {
    pub(crate) filename: &'data Path,
    pub(crate) original_filename: &'data Path,
    pub(crate) modifiers: Modifiers,
}

impl InputFileRef<'_> {
    #[cfg(test)]
    pub(crate) fn for_testing() -> Self {
        Self {
            filename: Path::new(""),
            original_filename: Path::new(""),
            modifiers: Modifiers::default(),
        }
    }
}

impl<I: InputFileData> InputFile<I> {
    pub(crate) fn data(&self) -> &[u8] {
        self.data.as_ref().map_or(&[], InputFileData::bytes)
    }

    pub(crate) fn as_ref(&self) -> InputFileRef<'_> {
        InputFileRef {
            filename: &self.filename,
            original_filename: &self.original_filename,
            modifiers: self.modifiers,
        }
    }
}

/// Identifies an input object that may not be a regular file on disk, or may be an entry in an
/// archive.
#[derive(Clone, Copy)]
pub(crate) struct InputRef<'data> {
    pub(crate) file: InputFileRef<'data>,
    pub(crate) data: &'data [u8],
    pub(crate) entry: Option<archive::EntryMeta<'data>>,
}

#[derive(Debug)]
pub(crate) struct InputPath {
    /// An absolute path to the file.
    pub(crate) absolute: PathBuf,

    /// The file as specified on the command line. In the case of an argument like -lfoo, this will
    /// be "libfoo.so".
    pub(crate) original: PathBuf,
}

#[derive(Debug)]
pub(crate) struct InputLinkerScript<'data> {
    pub(crate) script: LinkerScript<'data>,
    pub(crate) input_file: InputFileRef<'data>,
    /// Raw bytes of the script file. Used to compute line numbers from `AssertCommand::remainder`.
    pub(crate) script_bytes: &'data [u8],
}

pub(crate) struct AuxiliaryFiles<'data> {
    pub(crate) version_script_data: Option<ScriptData<'data>>,
    pub(crate) export_list_data: Option<ScriptData<'data>>,
}

const FILE_INDEX_BITS: u32 = 8;
pub(crate) const MAX_FILES_PER_GROUP: u32 = 1 << FILE_INDEX_BITS;

impl FileId {
    pub(crate) const fn new(group: u32, file: u32) -> Self {
        Self((group << FILE_INDEX_BITS) | file)
    }

    pub(crate) const fn from_encoded(v: u32) -> Self {
        Self(v)
    }

    pub(crate) fn group(self) -> usize {
        self.0 as usize >> FILE_INDEX_BITS
    }

    pub(crate) fn file(self) -> usize {
        self.0 as usize & ((1 << FILE_INDEX_BITS) - 1)
    }

    pub(crate) fn as_u32(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for InputRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.file.filename.display(), f)?;
        if let Some(entry) = &self.entry {
            std::fmt::Display::fmt(" @ ", f)?;
            std::fmt::Display::fmt(&String::from_utf8_lossy(entry.identifier.as_slice()), f)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for InputRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}/{})", self.0, self.group(), self.file())
    }
}

impl<'data> InputRef<'data> {
    pub(crate) fn lib_name(&self) -> &'data [u8] {
        self.file.original_filename.as_os_str().as_encoded_bytes()
    }

    pub(crate) fn has_archive_semantics(&self) -> bool {
        self.entry.is_some() || self.file.modifiers.archive_semantics
    }

    pub(crate) fn data(&self) -> &'data [u8] {
        self.data
    }

    pub(crate) fn is_archive_entry(&self) -> bool {
        self.entry.is_some()
    }
}

impl Display for InputBytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.input, f)
    }
}
