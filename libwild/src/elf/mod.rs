use crate::FileSystem;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::error::Result;
use crate::output_section_id::OutputSectionId;
use crate::part_id::PartId;

pub(crate) mod abi;
pub(crate) mod file;
pub(crate) mod gnu;
pub(crate) mod output;
pub(crate) mod strtab;
pub(crate) mod types;

#[allow(unused_imports)]
pub(crate) use abi::*;
#[allow(unused_imports)]
pub(crate) use file::*;
#[allow(unused_imports)]
pub(crate) use gnu::*;
#[allow(unused_imports)]
pub(crate) use output::*;
#[allow(unused_imports)]
pub(crate) use strtab::*;
#[allow(unused_imports)]
pub(crate) use types::*;

pub(crate) const GLOBAL_POINTER_SYMBOL_NAME: &str = "__global_pointer$";

/// The ppc64 TOC base symbol. Defined to point at the start of the GOT.
pub(crate) const TOC_SYMBOL_NAME: &str = ".TOC.";

pub(crate) const THUNK_SYMBOL_PREFIX: &str = "__thunk_";

pub(crate) fn link_for_arch<'data, F: FileSystem>(
    linker: &'data crate::Linker<F>,
    args: &'data ElfArgs,
) -> Result<crate::LinkerOutput<'data>> {
    match args.arch {
        crate::arch::Architecture::X86_64 => {
            linker.link_for_arch::<Elf64, crate::elf_x86_64::ElfX86_64>(args)
        }
        crate::arch::Architecture::AArch64 => {
            linker.link_for_arch::<Elf64, crate::elf_aarch64::ElfAArch64>(args)
        }
        crate::arch::Architecture::RiscV64 => {
            linker.link_for_arch::<Elf64, crate::elf_riscv64::ElfRiscV64>(args)
        }
        crate::arch::Architecture::LoongArch64 => {
            linker.link_for_arch::<Elf64, crate::elf_loongarch64::ElfLoongArch64>(args)
        }
        crate::arch::Architecture::Ppc64 => {
            linker.link_for_arch::<Elf64, crate::elf_ppc64::ElfPpc64>(args)
        }
        crate::arch::Architecture::Unsupported => {
            bail!(
                "No default target architecture known for host platform. \
                    Please specify an architecture with -m"
            )
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub(crate) enum SinglePartSectionId {
    ProgramHeaders = crate::output_section_id::NUM_COMMON_SINGLE_PART_SECTIONS,
    SectionHeaders,
    Shstrtab,
    Strtab,
    Got,
    GotRelr,
    PltGot,
    RelaPlt,
    EhFrame,
    EhFrameHdr,
    Sframe,
    Dynamic,
    SysvHash,
    GnuHash,
    Dynsym,
    Dynstr,
    Interp,
    GnuVersion,
    GnuVersionD,
    GnuVersionR,
    NoteGnuProperty,
    NoteGnuBuildId,
    SymtabLocal,
    SymtabGlobal,
    RelaDynRelative,
    RelaDynGeneral,
    RiscvAttributes,
    RelroPadding,
    RelrDyn,
    SymtabShndxLocal,
    SymtabShndxGlobal,
    GdbIndex,

    // Must be last.
    Count,
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub(crate) enum RegularSectionId {
    Rodata,
    InitArray,
    FiniArray,
    PreinitArray,
    Text,
    Init,
    Fini,
    Data,
    Tdata,
    Tbss,
    Bss,
    Comment,
    GccExceptTable,
    NoteAbiTag,
    DataRelRo,

    // Must be last.
    Count,
}

pub(crate) const ELF_NUM_SINGLE_PART_SECTIONS: u32 = SinglePartSectionId::Count as u32;
pub(crate) const ELF_NUM_BUILT_IN_REGULAR_SECTIONS: usize = RegularSectionId::Count as usize;
pub(crate) const ELF_NUM_BUILT_IN_SECTIONS: usize =
    ELF_NUM_SINGLE_PART_SECTIONS as usize + ELF_NUM_BUILT_IN_REGULAR_SECTIONS;

pub(crate) mod part_id {
    use super::SinglePartSectionId;
    use crate::part_id::PartId;

    pub(crate) const PROGRAM_HEADERS: PartId = SinglePartSectionId::ProgramHeaders.part_id();
    pub(crate) const SECTION_HEADERS: PartId = SinglePartSectionId::SectionHeaders.part_id();
    pub(crate) const SHSTRTAB: PartId = SinglePartSectionId::Shstrtab.part_id();
    pub(crate) const STRTAB: PartId = SinglePartSectionId::Strtab.part_id();
    pub(crate) const GOT: PartId = SinglePartSectionId::Got.part_id();
    pub(crate) const GOT_RELR: PartId = SinglePartSectionId::GotRelr.part_id();
    pub(crate) const PLT_GOT: PartId = SinglePartSectionId::PltGot.part_id();
    pub(crate) const RELA_PLT: PartId = SinglePartSectionId::RelaPlt.part_id();
    pub(crate) const EH_FRAME: PartId = SinglePartSectionId::EhFrame.part_id();
    pub(crate) const EH_FRAME_HDR: PartId = SinglePartSectionId::EhFrameHdr.part_id();
    pub(crate) const DYNAMIC: PartId = SinglePartSectionId::Dynamic.part_id();
    pub(crate) const SYSV_HASH: PartId = SinglePartSectionId::SysvHash.part_id();
    pub(crate) const GNU_HASH: PartId = SinglePartSectionId::GnuHash.part_id();
    pub(crate) const DYNSYM: PartId = SinglePartSectionId::Dynsym.part_id();
    pub(crate) const DYNSTR: PartId = SinglePartSectionId::Dynstr.part_id();
    pub(crate) const INTERP: PartId = SinglePartSectionId::Interp.part_id();
    pub(crate) const GNU_VERSION: PartId = SinglePartSectionId::GnuVersion.part_id();
    pub(crate) const GNU_VERSION_D: PartId = SinglePartSectionId::GnuVersionD.part_id();
    pub(crate) const GNU_VERSION_R: PartId = SinglePartSectionId::GnuVersionR.part_id();
    pub(crate) const NOTE_GNU_PROPERTY: PartId = SinglePartSectionId::NoteGnuProperty.part_id();
    pub(crate) const NOTE_GNU_BUILD_ID: PartId = SinglePartSectionId::NoteGnuBuildId.part_id();
    pub(crate) const SYMTAB_LOCAL: PartId = SinglePartSectionId::SymtabLocal.part_id();
    pub(crate) const SYMTAB_GLOBAL: PartId = SinglePartSectionId::SymtabGlobal.part_id();
    pub(crate) const RELA_DYN_RELATIVE: PartId = SinglePartSectionId::RelaDynRelative.part_id();
    pub(crate) const RELA_DYN_GENERAL: PartId = SinglePartSectionId::RelaDynGeneral.part_id();
    pub(crate) const RISCV_ATTRIBUTES: PartId = SinglePartSectionId::RiscvAttributes.part_id();
    pub(crate) const RELR_DYN: PartId = SinglePartSectionId::RelrDyn.part_id();
    pub(crate) const SYMTAB_SHNDX_LOCAL: PartId = SinglePartSectionId::SymtabShndxLocal.part_id();
    pub(crate) const SYMTAB_SHNDX_GLOBAL: PartId = SinglePartSectionId::SymtabShndxGlobal.part_id();
    pub(crate) const GDB_INDEX: PartId = SinglePartSectionId::GdbIndex.part_id();
}

pub(crate) mod output_section_id {
    use super::RegularSectionId;
    use super::SinglePartSectionId;
    use crate::output_section_id::OutputSectionId;

    pub(crate) const PROGRAM_HEADERS: OutputSectionId =
        SinglePartSectionId::ProgramHeaders.output_section_id();
    pub(crate) const SECTION_HEADERS: OutputSectionId =
        SinglePartSectionId::SectionHeaders.output_section_id();
    pub(crate) const SHSTRTAB: OutputSectionId = SinglePartSectionId::Shstrtab.output_section_id();
    pub(crate) const STRTAB: OutputSectionId = SinglePartSectionId::Strtab.output_section_id();
    pub(crate) const GOT: OutputSectionId = SinglePartSectionId::Got.output_section_id();
    pub(crate) const GOT_RELR: OutputSectionId = SinglePartSectionId::GotRelr.output_section_id();
    pub(crate) const PLT_GOT: OutputSectionId = SinglePartSectionId::PltGot.output_section_id();
    pub(crate) const RELA_PLT: OutputSectionId = SinglePartSectionId::RelaPlt.output_section_id();
    pub(crate) const EH_FRAME: OutputSectionId = SinglePartSectionId::EhFrame.output_section_id();
    pub(crate) const EH_FRAME_HDR: OutputSectionId =
        SinglePartSectionId::EhFrameHdr.output_section_id();
    pub(crate) const SFRAME: OutputSectionId = SinglePartSectionId::Sframe.output_section_id();
    pub(crate) const DYNAMIC: OutputSectionId = SinglePartSectionId::Dynamic.output_section_id();
    pub(crate) const HASH: OutputSectionId = SinglePartSectionId::SysvHash.output_section_id();
    pub(crate) const GNU_HASH: OutputSectionId = SinglePartSectionId::GnuHash.output_section_id();
    pub(crate) const DYNSYM: OutputSectionId = SinglePartSectionId::Dynsym.output_section_id();
    pub(crate) const DYNSTR: OutputSectionId = SinglePartSectionId::Dynstr.output_section_id();
    pub(crate) const INTERP: OutputSectionId = SinglePartSectionId::Interp.output_section_id();
    pub(crate) const GNU_VERSION: OutputSectionId =
        SinglePartSectionId::GnuVersion.output_section_id();
    pub(crate) const GNU_VERSION_D: OutputSectionId =
        SinglePartSectionId::GnuVersionD.output_section_id();
    pub(crate) const GNU_VERSION_R: OutputSectionId =
        SinglePartSectionId::GnuVersionR.output_section_id();
    pub(crate) const NOTE_GNU_PROPERTY: OutputSectionId =
        SinglePartSectionId::NoteGnuProperty.output_section_id();
    pub(crate) const NOTE_GNU_BUILD_ID: OutputSectionId =
        SinglePartSectionId::NoteGnuBuildId.output_section_id();
    pub(crate) const SYMTAB_LOCAL: OutputSectionId =
        SinglePartSectionId::SymtabLocal.output_section_id();
    pub(crate) const SYMTAB_GLOBAL: OutputSectionId =
        SinglePartSectionId::SymtabGlobal.output_section_id();
    pub(crate) const RELA_DYN_RELATIVE: OutputSectionId =
        SinglePartSectionId::RelaDynRelative.output_section_id();
    pub(crate) const RELA_DYN_GENERAL: OutputSectionId =
        SinglePartSectionId::RelaDynGeneral.output_section_id();
    pub(crate) const RISCV_ATTRIBUTES: OutputSectionId =
        SinglePartSectionId::RiscvAttributes.output_section_id();
    pub(crate) const RELRO_PADDING: OutputSectionId =
        SinglePartSectionId::RelroPadding.output_section_id();
    pub(crate) const RELR_DYN: OutputSectionId = SinglePartSectionId::RelrDyn.output_section_id();
    pub(crate) const SYMTAB_SHNDX_LOCAL: OutputSectionId =
        SinglePartSectionId::SymtabShndxLocal.output_section_id();
    pub(crate) const SYMTAB_SHNDX_GLOBAL: OutputSectionId =
        SinglePartSectionId::SymtabShndxGlobal.output_section_id();
    pub(crate) const GDB_INDEX: OutputSectionId = SinglePartSectionId::GdbIndex.output_section_id();

    pub(crate) const RODATA: OutputSectionId = RegularSectionId::Rodata.output_section_id();
    pub(crate) const INIT_ARRAY: OutputSectionId = RegularSectionId::InitArray.output_section_id();
    pub(crate) const FINI_ARRAY: OutputSectionId = RegularSectionId::FiniArray.output_section_id();
    pub(crate) const PREINIT_ARRAY: OutputSectionId =
        RegularSectionId::PreinitArray.output_section_id();
    pub(crate) const TEXT: OutputSectionId = RegularSectionId::Text.output_section_id();
    pub(crate) const INIT: OutputSectionId = RegularSectionId::Init.output_section_id();
    pub(crate) const FINI: OutputSectionId = RegularSectionId::Fini.output_section_id();
    pub(crate) const DATA: OutputSectionId = RegularSectionId::Data.output_section_id();
    pub(crate) const TDATA: OutputSectionId = RegularSectionId::Tdata.output_section_id();
    pub(crate) const TBSS: OutputSectionId = RegularSectionId::Tbss.output_section_id();
    pub(crate) const BSS: OutputSectionId = RegularSectionId::Bss.output_section_id();
    pub(crate) const COMMENT: OutputSectionId = RegularSectionId::Comment.output_section_id();
    pub(crate) const GCC_EXCEPT_TABLE: OutputSectionId =
        RegularSectionId::GccExceptTable.output_section_id();
    pub(crate) const NOTE_ABI_TAG: OutputSectionId =
        RegularSectionId::NoteAbiTag.output_section_id();
    pub(crate) const DATA_REL_RO: OutputSectionId = RegularSectionId::DataRelRo.output_section_id();
}

impl SinglePartSectionId {
    const fn part_id(self) -> PartId {
        PartId::from_u32(self as u32)
    }

    const fn output_section_id(self) -> OutputSectionId {
        OutputSectionId::from_u32(self as u32)
    }
}

impl RegularSectionId {
    const fn output_section_id(self) -> OutputSectionId {
        OutputSectionId::from_u32(ELF_NUM_SINGLE_PART_SECTIONS).offset(self as usize)
    }
}
