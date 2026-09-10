//! Input-file records and the loader handle. Loading (sniffing, archives, plugins) stays in
//! libwild so this crate does not depend on format parsers.

use colosseum::sync::Arena;
use std::path::PathBuf;
use std::sync::Arc;
use wild_args::InputFileRef;
use wild_args::Modifiers;
use wild_fs::fs::FileSystem;
use wild_fs::fs::InputFileData;
use wild_scripts::ScriptData;

pub struct FileLoader<'data, F: FileSystem> {
    /// The files that we've loaded so far.
    pub loaded_files: Vec<&'data InputFile<F::Input>>,

    /// Whether we have at least one input file that is a dynamic object.
    pub has_dynamic: bool,

    pub inputs_arena: &'data Arena<InputFile<F::Input>>,

    pub file_system: Arc<F>,
}

impl<'data, F: FileSystem> FileLoader<'data, F> {
    pub fn new(inputs_arena: &'data Arena<InputFile<F::Input>>, file_system: Arc<F>) -> Self {
        Self {
            loaded_files: Vec::new(),
            inputs_arena,
            file_system,
            has_dynamic: false,
        }
    }
}

#[derive(Debug)]
pub struct InputFile<D: InputFileData> {
    pub filename: PathBuf,

    /// The filename prior to path search. If this is absolute, then `filename` will be the same.
    pub original_filename: PathBuf,

    pub modifiers: Modifiers,

    pub data: Option<D>,
}

impl<I: InputFileData> InputFile<I> {
    pub fn data(&self) -> &[u8] {
        self.data.as_ref().map_or(&[], InputFileData::bytes)
    }

    pub fn as_ref(&self) -> InputFileRef<'_> {
        InputFileRef {
            filename: &self.filename,
            original_filename: &self.original_filename,
            modifiers: self.modifiers,
        }
    }
}

#[derive(Debug)]
pub struct InputPath {
    /// An absolute path to the file.
    pub absolute: PathBuf,

    /// The file as specified on the command line. In the case of an argument like -lfoo, this will
    /// be "libfoo.so".
    pub original: PathBuf,
}

pub struct AuxiliaryFiles<'data> {
    pub version_script_data: Option<ScriptData<'data>>,
    pub export_list_data: Option<ScriptData<'data>>,
}
