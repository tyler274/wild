#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::gnu::*;
use crate::elf::output_section_id;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::layout;
use crate::layout_rules::SectionRule;
use crate::layout_rules::SectionRuleOutcome;
use linker_utils::elf::secnames;

/// Rules that map input sections to built-in output sections when no linker script is in use.
pub(crate) const DEFAULT_SECTION_PLACEMENT_RULES: &[SectionRule<'static>] = &[
    SectionRule::exact_section_keep(secnames::INIT_SECTION_NAME, output_section_id::INIT),
    SectionRule::exact_section_keep(secnames::FINI_SECTION_NAME, output_section_id::FINI),
    SectionRule::exact_section_keep(
        secnames::PREINIT_ARRAY_SECTION_NAME,
        output_section_id::PREINIT_ARRAY,
    ),
    SectionRule::exact_section_keep(secnames::COMMENT_SECTION_NAME, output_section_id::COMMENT),
    SectionRule::exact_section_keep(
        secnames::NOTE_ABI_TAG_SECTION_NAME,
        output_section_id::NOTE_ABI_TAG,
    ),
    SectionRule::exact_section(
        secnames::NOTE_GNU_BUILD_ID_SECTION_NAME,
        output_section_id::NOTE_GNU_BUILD_ID,
    ),
    SectionRule::prefix_section(secnames::RODATA_SECTION_NAME, output_section_id::RODATA),
    SectionRule::prefix_section(secnames::TEXT_SECTION_NAME, output_section_id::TEXT),
    SectionRule::prefix_section(
        secnames::DATA_REL_RO_SECTION_NAME,
        output_section_id::DATA_REL_RO,
    ),
    SectionRule::prefix_section(secnames::DATA_SECTION_NAME, output_section_id::DATA),
    SectionRule::prefix_section(secnames::BSS_SECTION_NAME, output_section_id::BSS),
    SectionRule::prefix_section_sort(
        secnames::INIT_ARRAY_SECTION_NAME,
        output_section_id::INIT_ARRAY,
    ),
    SectionRule::prefix_section_sort(secnames::CTORS_SECTION_NAME, output_section_id::INIT_ARRAY),
    SectionRule::prefix_section_sort(
        secnames::FINI_ARRAY_SECTION_NAME,
        output_section_id::FINI_ARRAY,
    ),
    SectionRule::prefix_section_sort(secnames::DTORS_SECTION_NAME, output_section_id::FINI_ARRAY),
    SectionRule::prefix_section(secnames::TDATA_SECTION_NAME, output_section_id::TDATA),
    SectionRule::prefix_section(secnames::TBSS_SECTION_NAME, output_section_id::TBSS),
    SectionRule::prefix_section(
        secnames::GCC_EXCEPT_TABLE_SECTION_NAME,
        output_section_id::GCC_EXCEPT_TABLE,
    ),
];

/// Rules for input sections that the linker processes itself instead of copying them into an output
/// section.
pub(crate) const LINKER_MANAGED_SECTION_RULES: &[SectionRule<'static>] = &[
    SectionRule::prefix(secnames::RELA_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::prefix(secnames::CREL_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(
        secnames::NOTE_GNU_STACK_SECTION_NAME,
        SectionRuleOutcome::NoteGnuStack,
    ),
    SectionRule::exact(secnames::STRTAB_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::SYMTAB_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::SHSTRTAB_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::GROUP_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::EH_FRAME_SECTION_NAME, SectionRuleOutcome::EhFrame),
    SectionRule::exact(
        secnames::NOTE_GNU_PROPERTY_SECTION_NAME,
        SectionRuleOutcome::NoteGnuProperty,
    ),
    SectionRule::exact(
        secnames::RISCV_ATTRIBUTES_SECTION_NAME,
        SectionRuleOutcome::RiscVAttribute,
    ),
    SectionRule::exact(
        secnames::SYMTAB_SHNDX_SECTION_NAME,
        SectionRuleOutcome::Discard,
    ),
    SectionRule::prefix(b".debug_", SectionRuleOutcome::Debug),
];

pub(crate) fn init_fini_priority(name: &[u8]) -> Option<u16> {
    if name == secnames::INIT_ARRAY_SECTION_NAME || name == secnames::FINI_ARRAY_SECTION_NAME {
        return Some(u16::MAX);
    }

    if let Some(rest) = name.strip_prefix(b".init_array.") {
        return parse_priority_suffix(rest);
    }

    if let Some(rest) = name.strip_prefix(b".fini_array.") {
        return parse_priority_suffix(rest);
    }

    // .ctors and .dtors without suffix have the same priority as .init_array/.fini_array
    if name == secnames::CTORS_SECTION_NAME || name == secnames::DTORS_SECTION_NAME {
        return Some(u16::MAX);
    }

    // .ctors uses descending order (65535 = lowest priority, 0 = highest)
    // while .init_array uses ascending order (0 = highest priority, 65535 = lowest)
    if let Some(rest) = name.strip_prefix(b".ctors.") {
        return parse_priority_suffix(rest).map(|p| u16::MAX.saturating_sub(p));
    }

    if let Some(rest) = name.strip_prefix(b".dtors.") {
        return parse_priority_suffix(rest).map(|p| u16::MAX.saturating_sub(p));
    }

    None
}

pub(crate) fn parse_priority_suffix(suffix: &[u8]) -> Option<u16> {
    if suffix.is_empty() || !suffix.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let value = core::str::from_utf8(suffix).ok()?.parse::<u32>().ok()?;
    Some(u16::try_from(value).unwrap_or(u16::MAX))
}

pub(crate) fn program_headers_size<C: ElfClass>(header_info: &layout::HeaderInfo) -> u64 {
    u64::from(C::PROGRAM_HEADER_SIZE) * header_info.active_segment_ids.len() as u64
}

pub(crate) fn section_headers_size<C: ElfClass>(header_info: &layout::HeaderInfo) -> u64 {
    u64::from(C::SECTION_HEADER_SIZE) * u64::from(header_info.num_output_sections_with_content)
}
