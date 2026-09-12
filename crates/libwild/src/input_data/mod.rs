//! Code for figuring out what input files we need to read then mapping them into memory.

pub(crate) mod load;

#[allow(unused_imports)]
pub(crate) use load::*;
pub(crate) use wild_args::{InputLinkerScript, InputRef};
pub(crate) use wild_layout::input_data::{AuxiliaryFiles, FileLoader, InputFile, InputPath};
pub(crate) use wild_scripts::ScriptData;
