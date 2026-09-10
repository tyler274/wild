use crate::FileSystem;
use crate::InputFileData;
pub(crate) use crate::args::InputFileRef;
pub(crate) use crate::args::InputLinkerScript;
pub(crate) use crate::args::InputRef;
use crate::args::Modifiers;
use colosseum::sync::Arena;
use std::path::PathBuf;
use std::sync::Arc;
#[allow(unused_imports)]
pub(crate) use wild_platform::file_id::*;

pub(crate) struct FileLoader<'data, F: FileSystem> {
    /// The files that we've loaded so far.
    pub(crate) loaded_files: Vec<&'data InputFile<F::Input>>,

    /// Whether we have at least one input file that is a dynamic object.
    pub(crate) has_dynamic: bool,

    pub(crate) inputs_arena: &'data Arena<InputFile<F::Input>>,

    // File system used for reading and writing of the data.
    pub(crate) file_system: Arc<F>,
}

pub(crate) use wild_scripts::ScriptData;

#[derive(Debug)]
pub(crate) struct InputFile<D: InputFileData> {
    pub(crate) filename: PathBuf,

    /// The filename prior to path search. If this is absolute, then `filename` will be the same.
    pub(crate) original_filename: PathBuf,

    pub(crate) modifiers: Modifiers,

    pub(crate) data: Option<D>,
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

#[derive(Debug)]
pub(crate) struct InputPath {
    /// An absolute path to the file.
    pub(crate) absolute: PathBuf,

    /// The file as specified on the command line. In the case of an argument like -lfoo, this will
    /// be "libfoo.so".
    pub(crate) original: PathBuf,
}

pub(crate) struct AuxiliaryFiles<'data> {
    pub(crate) version_script_data: Option<ScriptData<'data>>,
    pub(crate) export_list_data: Option<ScriptData<'data>>,
}
