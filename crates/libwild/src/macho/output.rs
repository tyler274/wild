use super::MachO;
#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::file::*;
use super::output_section_id;
use super::part_id;
#[allow(unused_imports)]
use super::types::*;
use crate::alignment;
use crate::alignment::Alignment;
use crate::args::macho::MachOArgs;
use crate::error::Result;
use crate::grouping::SequencedInput;
use crate::input_data::FileId;
use crate::layout;
use crate::layout::Layout;
use crate::layout::OutputRecordLayout;
use crate::layout_rules::SectionKind;
use crate::layout_rules::SectionRule;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrderBuilder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform;
use crate::platform::ObjectFile;
use crate::program_segments::ProgramSegmentId;
use crate::symbol_db::SymbolId;
use crate::value_flags::ValueFlags;
use anyhow::Context;
use object::SymbolIndex;
use object::macho;
use object::macho::SEG_LINKEDIT;
pub use object::macho::SectionFlags;
use std::num::NonZeroU8;
use std::num::NonZeroU64;

pub(crate) fn install_name<'data>(
    file_id: FileId,
    symbol_db: &crate::symbol_db::SymbolDb<'data, MachO>,
) -> &'data [u8] {
    match symbol_db.file(file_id) {
        SequencedInput::StubLibrary(stub) => stub.defined_symbols.install_name.as_bytes(),
        SequencedInput::Object(obj) => obj.parsed.input.lib_name(),
        _ => {
            panic!("Internal error: Expected StubLibrary or Dynamic");
        }
    }
}

pub(super) fn create_dynamic_layout_ext<'data>(
    target_file_id: FileId,
    resources: &layout::FinaliseLayoutResources<'_, 'data, MachO>,
) -> Result<Option<DynamicLayoutExt>> {
    let Some(index) = resources
        .format_specific
        .imported_libraries
        .iter()
        .position(|file_id| *file_id == target_file_id)
    else {
        return Ok(None);
    };

    Ok(Some(DynamicLayoutExt {
        ordinal: NonZeroU8::new(u8::try_from(index + 1).context("Too many loaded stub libraries")?)
            .unwrap(),
    }))
}

pub(super) const NUM_BUILT_IN_SECTIONS: usize =
    crate::output_section_id::num_built_in_sections::<MachO>();

pub(super) const SECTION_DEFINITIONS: [BuiltInSectionDetails; NUM_BUILT_IN_SECTIONS] = {
    let mut defs = [DEFAULT_DEFS; NUM_BUILT_IN_SECTIONS];

    defs[crate::output_section_id::FILE_HEADER.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"FILE_HEADER"), None)),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::LOAD_COMMANDS.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"LOAD_COMMANDS"), None)),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::LINK_EDIT_SEGMENT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(
            SectionName(SEG_LINKEDIT.as_bytes()),
            None,
        )),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::STRTAB.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"STRTAB"), None)),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::CHAINED_FIXUP_TABLE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(
            SectionName(b"DYLD_CHAINED_FIXUPS_TABLE"),
            None,
        )),
        min_alignment: alignment::USIZE,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::EXPORTS_TRIE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"EXPORTS_TRIE"), None)),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::SYMTAB_GLOBAL.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"SYMTAB"), None)),
        min_alignment: alignment::USIZE,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::CODE_SIGNATURE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(SectionName(b"CODE_SIGNATURE"), None)),
        min_alignment: Alignment {
            exponent: CS_SECTION_ALIGNMENT_EXP,
        },
        ..DEFAULT_DEFS
    };
    defs[output_section_id::GOT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(
            SectionName(b"__got"),
            Some(SegmentName::DATA_CONST),
        )),
        section_flags: macho::S_NON_LAZY_SYMBOL_POINTERS.to_flags(),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::PLT_GOT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionIdentity::new(
            SectionName(b"__stubs"),
            Some(SegmentName::TEXT),
        )),
        section_flags: macho::S_SYMBOL_STUBS
            .to_flags()
            .with(macho::S_ATTR_PURE_INSTRUCTIONS)
            .with(macho::S_ATTR_SOME_INSTRUCTIONS),
        min_alignment: Alignment { exponent: 2 },
        ..DEFAULT_DEFS
    };

    defs
};

#[derive(Debug, Default)]
pub(crate) struct EpilogueLayoutExt {
    pub(super) imported_symbols: Vec<SymbolId>,
}

#[derive(Debug)]
pub(crate) struct DynamicLayoutStateExt {
    pub(super) imported_symbols: Vec<SymbolId>,
    pub(super) loaded: bool,
}

#[derive(Debug)]
pub(crate) struct DynamicLayoutExt {
    pub(crate) ordinal: NonZeroU8,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResolutionExt {
    pub(crate) got_address: Option<NonZeroU64>,
    pub(crate) plt_address: Option<NonZeroU64>,
}

pub(super) fn allocate_got(memory_offsets: &mut OutputSectionPartMap<u64>) -> NonZeroU64 {
    let got_address = NonZeroU64::new(memory_offsets.get(part_id::GOT)).unwrap();
    memory_offsets.increment(part_id::GOT, GOT_ENTRY_SIZE);
    got_address
}

pub(super) fn allocate_plt(memory_offsets: &mut OutputSectionPartMap<u64>) -> NonZeroU64 {
    let plt_address = NonZeroU64::new(memory_offsets.get(part_id::PLT_GOT)).unwrap();
    memory_offsets.increment(part_id::PLT_GOT, PLT_ENTRY_SIZE);
    plt_address
}

pub(super) const DEFAULT_SECTION_RULES: &[SectionRule<'static>] = &[
    // TODO: Add a Mach-O output section ID and rule for `__compact_unwind`.
];

pub(super) fn section_header_name_for_segment<'data>(
    output_sections: &crate::output_section_id::OutputSections<'data, MachO>,
    section_id: OutputSectionId,
    segment_def: ProgramSegmentDef,
) -> Option<SectionName<'data>> {
    if !output_sections.will_emit_section(section_id) {
        return None;
    }

    output_sections
        .identity(section_id)
        .filter(|identity| identity.format_specific().is_some())
        .filter(|_| output_sections.should_include_in_segment(section_id, segment_def))
        .map(|identity| identity.section_name())
}

pub(super) fn count_sections_for_segment(
    output_sections: &crate::output_section_id::OutputSections<MachO>,
    segment_def: ProgramSegmentDef,
) -> usize {
    output_sections
        .ids_with_info()
        .filter(|(section_id, _)| {
            section_header_name_for_segment(output_sections, *section_id, segment_def).is_some()
        })
        .count()
}

pub(crate) fn get_segment_sections<'data>(
    layout: &Layout<'data, MachO>,
    segment_id: ProgramSegmentId,
) -> Vec<(OutputRecordLayout, SectionName<'data>, SectionFlags)> {
    let mut in_matching_segment = false;
    let mut segment_sections = Vec::new();
    let segment_def = *layout.program_segments.segment_def(segment_id);

    for event in &layout.output_order {
        match event {
            OrderEvent::SegmentStart(seg_id) if seg_id == segment_id => {
                in_matching_segment = true;
            }
            OrderEvent::SegmentEnd(seg_id) if seg_id == segment_id && in_matching_segment => {
                break;
            }
            OrderEvent::Section(section_id) if in_matching_segment => {
                let Some(section_name) = section_header_name_for_segment(
                    &layout.output_sections,
                    section_id,
                    segment_def,
                ) else {
                    continue;
                };

                segment_sections.push((
                    *layout.merged_section_layouts.get(section_id),
                    section_name,
                    layout.output_sections.section_flags(section_id),
                ));
            }
            _ => {}
        }
    }

    segment_sections
}

pub(super) fn add_sections_in_segment<'data>(
    builder: &mut OutputOrderBuilder<'_, 'data, MachO>,
    output_sections: &crate::output_section_id::OutputSections<'data, MachO>,
    sections: &[OutputSectionId],
    segment: SegmentName,
) {
    for &section_id in sections {
        if output_sections
            .identity(section_id)
            .is_some_and(|identity| identity.format_specific() == Some(segment))
        {
            builder.add_section(section_id);
        }
    }
}

#[inline(always)]
pub(super) fn process_relocation<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    object: &layout::ObjectLayoutState<'data, MachO>,
    rel: &Relocation,
    section_index: object::SectionIndex,
    resources: &'scope layout::GraphResources<'data, '_, MachO>,
    queue: &mut layout::LocalWorkQueue<MachO>,
    scope: &rayon::Scope<'scope>,
) -> Result {
    let rel_info = rel.info(LE);
    // r_extern == true if the reference points to a symbol
    if rel_info.r_extern {
        let local_sym_index = SymbolIndex(rel_info.r_symbolnum as usize);
        let symbol_db = resources.symbol_db;
        let local_symbol_id = object.symbol_id_range.input_to_id(local_sym_index);
        let symbol_id = symbol_db.definition(local_symbol_id);
        let mut flags = resources.local_flags_for_symbol(symbol_id);
        flags.merge(resources.local_flags_for_symbol(local_symbol_id));

        let relocation = A::relocation_from_raw(rel_info)?;
        let mut flags_to_add = layout::resolution_flags(relocation.kind);
        if is_dynamic_library(&symbol_db.file(symbol_db.file_id_for_symbol(symbol_id))) {
            flags_to_add |= ValueFlags::GOT;
            // TODO: classify symbols more reliably, likely by checking whether their section is
            // __text.
            if rel_info.r_type == object::macho::ARM64_RELOC_BRANCH26 {
                flags_to_add |= ValueFlags::DYNAMIC_FUNCTION | ValueFlags::PLT;
            }
        }

        let atomic_flags = &resources.per_symbol_flags.get_atomic(symbol_id);
        let previous_flags = atomic_flags.fetch_or(flags_to_add);

        layout::check_for_undefined::<A>(
            object,
            object.object.section(section_index)?,
            rel_info.r_address.into(),
            local_sym_index,
            flags,
            symbol_id,
            resources,
        )?;

        if !previous_flags.has_resolution() {
            queue.send_symbol_request::<A>(symbol_id, resources, scope);
        }
    }

    Ok(())
}

pub(super) fn is_dynamic_library(file: &SequencedInput<MachO>) -> bool {
    match file {
        SequencedInput::StubLibrary(_) => true,
        SequencedInput::Object(obj) => obj.is_dynamic(),
        _ => false,
    }
}

impl DynamicLayoutStateExt {
    pub(super) fn new(args: &MachOArgs) -> Self {
        Self {
            imported_symbols: Default::default(),
            loaded: !args.dead_strip_dylibs,
        }
    }
}
