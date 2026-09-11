use crate::wasm::Wasm;
use crate::wasm::relocation_type_to_string;
use wasmparser::RelocationType;
use wild_platform::PreviousRelocationInfo;

pub(crate) struct WasmWasm32;

#[derive(Debug, Clone)]
pub(crate) struct Relaxation {}

impl wild_platform::Relaxation for Relaxation {
    fn apply(&self, _section_bytes: &mut [u8], _offset_in_section: &mut u64, _addend: &mut i64) {
        unreachable!()
    }

    fn rel_info(&self) -> linker_utils::elf::RelocationKindInfo {
        unreachable!()
    }

    fn debug_kind(&self) -> impl std::fmt::Debug {
        unreachable!()
    }

    fn next_modifier(&self) -> linker_utils::relaxation::RelocationModifier {
        unreachable!()
    }

    fn is_mandatory(&self) -> bool {
        false
    }
}

impl wild_platform::Arch for WasmWasm32 {
    type Relaxation = Relaxation;
    type Platform = Wasm;

    fn arch_identifier() -> <Self::Platform as wild_platform::Platform>::ArchIdentifier {}

    fn get_dynamic_relocation_type(
        _relocation: linker_utils::elf::DynamicRelocationKind,
    ) -> RelocationType {
        todo!()
    }

    fn write_plt_entry(
        _plt_entry: &mut [u8],
        _got_address: u64,
        _plt_address: u64,
    ) -> crate::error::Result {
        // Wasm has no PLT.
        unreachable!("wasm has no PLT")
    }

    fn relocation_from_raw(
        _r_type: <Self::Platform as wild_platform::Platform>::RelocationInfo,
    ) -> crate::error::Result<linker_utils::elf::RelocationKindInfo> {
        // TODO: map Wasm reloc type codes (R_WASM_*) to RelocationKindInfo.
        todo!()
    }

    fn rel_type_to_string(r_type: RelocationType) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(relocation_type_to_string(r_type))
    }

    fn tp_offset_start(_layout: &wild_layout::Layout<Self::Platform>) -> u64 {
        // Wasm has no TLS yet.
        0
    }

    fn get_property_class(_property_type: u32) -> Option<wild_platform::PropertyClass> {
        // Wasm has no GNU property notes.
        None
    }

    fn merge_eflags(_eflags: impl Iterator<Item = u32>) -> crate::error::Result<u32> {
        // Wasm has no e_flags equivalent.
        Ok(0)
    }

    fn high_part_relocations() -> &'static [RelocationType] {
        &[]
    }

    fn get_source_info<'data>(
        _object: &<Self::Platform as wild_platform::Platform>::File<'data>,
        _relocations: &<Self::Platform as wild_platform::Platform>::RelocationSections,
        _section: &<Self::Platform as wild_platform::Platform>::SectionHeader,
        _offset_in_section: u64,
    ) -> crate::error::Result<wild_platform::SourceInfo> {
        todo!()
    }

    fn new_relaxation(
        _relocation_kind: RelocationType,
        _section_bytes: &[u8],
        _offset_in_section: u64,
        _flags: wild_platform::value_flags::ValueFlags,
        _output_kind: wild_platform::OutputKind,
        _section_flags: <Self::Platform as wild_platform::Platform>::SectionFlags,
        _relax_deltas: Option<&linker_utils::relaxation::SectionRelaxDeltas>,
        _sym_addr: u64,
        _section_address: u64,
        _rel_addend: i64,
        _previous_relocation: Option<PreviousRelocationInfo<RelocationType>>,
    ) -> Option<Self::Relaxation> {
        // Wasm doesn't currently support any relaxations.
        None
    }
}
