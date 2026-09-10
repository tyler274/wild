//! Code for figuring out what input files we need to read then mapping them into memory.

pub(crate) mod load;

#[allow(unused_imports)]
pub(crate) use load::*;
pub(crate) use wild_args::InputLinkerScript;
pub(crate) use wild_args::InputRef;
pub(crate) use wild_layout::input_data::AuxiliaryFiles;
pub(crate) use wild_layout::input_data::FileLoader;
pub(crate) use wild_layout::input_data::InputFile;
pub(crate) use wild_layout::input_data::InputPath;
pub(crate) use wild_platform::file_id::*;
pub(crate) use wild_scripts::ScriptData;
