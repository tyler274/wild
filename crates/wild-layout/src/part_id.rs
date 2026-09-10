use crate::platform;
#[allow(unused_imports)]
pub use crate::platform::part_id::*;

/// Returns whether the supplied section meets our criteria for section merging. Section merging is
/// optional. `SHF_MERGE|SHF_STRINGS` is merged at any alignment; strings are padded to that
/// alignment and identical strings from different alignments are not deduped. Non-string
/// `SHF_MERGE` (constants) is merged at any alignment; sections with `sh_entsize > 1` are split
/// into that many bytes so duplicate `.rodata.cst8` / `.rodata.cst16` units can share storage.
/// Inputs that have relocations are not merged (GNU ld concatenates them).
pub fn should_merge_sections(
    section_header: &impl platform::SectionHeader,
    _section_alignment: u64,
    args: &impl platform::Args,
) -> bool {
    args.should_merge_sections() && section_header.is_merge_section()
}
