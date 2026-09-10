//! Identifies what sort of file we're dealing with. Sniffing of file bytes lives with the
//! loaders so this crate does not parse ELF/Mach-O/Wasm.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FileKind {
    ElfObject,
    ElfDynamic,
    MachOObject,
    MachODylib,
    FatMachOObject,
    MachOStubLibrary,
    WasmObject,
    Archive,
    ThinArchive,
    Text,
    LlvmIr,
    GccIr,
}

impl FileKind {
    pub fn is_compiler_ir(self) -> bool {
        matches!(self, FileKind::LlvmIr | FileKind::GccIr)
    }

    pub fn is_dynamic(self) -> bool {
        matches!(self, FileKind::ElfDynamic | FileKind::MachODylib)
    }
}

impl std::fmt::Display for FileKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FileKind::ElfObject => "ELF object",
            FileKind::ElfDynamic => "ELF dynamic",
            FileKind::MachOObject => "Mach-O object",
            FileKind::MachODylib => "Mach-O dylib",
            FileKind::WasmObject => "Wasm object",
            FileKind::FatMachOObject => "Fat Mach-O object",
            FileKind::MachOStubLibrary => "Mach-O TBD library",
            FileKind::Archive => "archive",
            FileKind::ThinArchive => "thin archive",
            FileKind::Text => "text",
            FileKind::LlvmIr => "LLVM-IR",
            FileKind::GccIr => "GCC-IR",
        };
        std::fmt::Display::fmt(s, f)
    }
}
