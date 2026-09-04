use crate::FileSystem;
use crate::args::macho::MachOArgs;
use crate::error::Result;
use crate::output_section_id::OutputSectionId;
use crate::part_id::PartId;
use crate::platform::Args as _;

pub(crate) mod abi;
pub(crate) mod file;
pub(crate) mod output;
pub(crate) mod types;

#[allow(unused_imports)]
pub(crate) use abi::*;
#[allow(unused_imports)]
pub(crate) use file::*;
pub(crate) use object::Endianness;
pub use object::macho::SectionFlags;
#[allow(unused_imports)]
pub(crate) use output::*;
pub(crate) use types::*;

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct MachO;

impl crate::layout::EnginePlatform for MachO {}
impl<'data, 'scope> crate::layout::EngineScope<'data, 'scope> for MachO where 'data: 'scope {}
impl<'writer, 'out> crate::layout::EngineWriter<'writer, 'out> for MachO where 'out: 'writer {}

pub(crate) fn link_for_arch<'data, F: FileSystem>(
    linker: &'data crate::Linker<F>,
    args: &'data MachOArgs,
) -> Result<crate::LinkerOutput<'data>> {
    if !(cfg!(feature = "macho") || args.common().experimental_platforms) {
        crate::bail!(
            "Mach-O support is still experimental. Rebuild with `--features macho` to enable it."
        );
    }

    linker.link_for_arch::<MachO, crate::macho_aarch64::MachOAArch64>(args)
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub(crate) enum SinglePartSectionId {
    Strtab = crate::output_section_id::NUM_COMMON_SINGLE_PART_SECTIONS,
    Got,
    PltGot,
    SymtabGlobal,
    LinkEditSegment,
    LoadCommands,
    CodeSignature,
    ChainedFixupTable,
    ExportsTrie,

    // Must be last.
    Count,
}

pub(crate) mod part_id {
    use super::SinglePartSectionId;
    use crate::part_id::PartId;

    pub(crate) const STRTAB: PartId = SinglePartSectionId::Strtab.part_id();
    pub(crate) const GOT: PartId = SinglePartSectionId::Got.part_id();
    pub(crate) const PLT_GOT: PartId = SinglePartSectionId::PltGot.part_id();
    pub(crate) const SYMTAB_GLOBAL: PartId = SinglePartSectionId::SymtabGlobal.part_id();
    pub(crate) const LOAD_COMMANDS: PartId = SinglePartSectionId::LoadCommands.part_id();
    pub(crate) const CODE_SIGNATURE: PartId = SinglePartSectionId::CodeSignature.part_id();
    pub(crate) const CHAINED_FIXUP_TABLE: PartId = SinglePartSectionId::ChainedFixupTable.part_id();
    pub(crate) const EXPORTS_TRIE: PartId = SinglePartSectionId::ExportsTrie.part_id();
}

pub(crate) mod output_section_id {
    use super::SinglePartSectionId;
    use crate::output_section_id::OutputSectionId;

    pub(crate) const STRTAB: OutputSectionId = SinglePartSectionId::Strtab.output_section_id();
    pub(crate) const GOT: OutputSectionId = SinglePartSectionId::Got.output_section_id();
    pub(crate) const PLT_GOT: OutputSectionId = SinglePartSectionId::PltGot.output_section_id();
    pub(crate) const SYMTAB_GLOBAL: OutputSectionId =
        SinglePartSectionId::SymtabGlobal.output_section_id();
    pub(crate) const LINK_EDIT_SEGMENT: OutputSectionId =
        SinglePartSectionId::LinkEditSegment.output_section_id();
    pub(crate) const LOAD_COMMANDS: OutputSectionId =
        SinglePartSectionId::LoadCommands.output_section_id();
    pub(crate) const CODE_SIGNATURE: OutputSectionId =
        SinglePartSectionId::CodeSignature.output_section_id();
    pub(crate) const CHAINED_FIXUP_TABLE: OutputSectionId =
        SinglePartSectionId::ChainedFixupTable.output_section_id();
    pub(crate) const EXPORTS_TRIE: OutputSectionId =
        SinglePartSectionId::ExportsTrie.output_section_id();
}

impl SinglePartSectionId {
    const fn part_id(self) -> PartId {
        PartId::from_u32(self as u32)
    }

    const fn output_section_id(self) -> OutputSectionId {
        OutputSectionId::from_u32(self as u32)
    }
}
