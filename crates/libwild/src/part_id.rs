use crate::platform;
#[allow(unused_imports)]
pub(crate) use crate::platform::part_id::*;

/// Returns whether the supplied section meets our criteria for section merging. Section merging is
/// optional. `SHF_MERGE|SHF_STRINGS` is merged at any alignment; strings are padded to that
/// alignment and identical strings from different alignments are not deduped. Non-string
/// `SHF_MERGE` (constants) is merged at any alignment; sections with `sh_entsize > 1` are split
/// into that many bytes so duplicate `.rodata.cst8` / `.rodata.cst16` units can share storage.
/// Inputs that have relocations are not merged (GNU ld concatenates them).
pub(crate) fn should_merge_sections(
    section_header: &impl platform::SectionHeader,
    _section_alignment: u64,
    args: &impl platform::Args,
) -> bool {
    args.should_merge_sections() && section_header.is_merge_section()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::RelocationModel;
    use crate::output_kind::OutputKind;
    use crate::output_section_id;
    use crate::output_section_id::OutputSectionId;
    use crate::output_section_id::OutputSections;

    fn check_platform_part_ids<P: crate::layout::EnginePlatform>() {
        let output_kind = OutputKind::StaticExecutable(RelocationModel::Fixed);
        let output_sections = OutputSections::<P>::with_base_address(0, output_kind);
        let regular_part_base = regular_part_base::<P>();
        let regular_section_base = output_section_id::regular_section_base::<P>();
        let num_single_part_sections = P::NUM_SINGLE_PART_SECTIONS as usize;

        for section_id in (0..num_single_part_sections).map(OutputSectionId::from_usize) {
            let part_id = P::single_part_id(section_id).unwrap();
            assert_eq!(P::single_part_output_section_id(part_id), Some(section_id));
            assert_eq!(
                section_id.base_part_id::<P>(),
                part_id,
                "single-part base ID failed for {}",
                std::any::type_name::<P>()
            );
            assert_eq!(
                part_id.output_section_id::<P>(),
                section_id,
                "single-part round trip failed for {}",
                std::any::type_name::<P>()
            );
        }

        assert_eq!(P::single_part_id(regular_section_base), None);
        assert_eq!(P::single_part_output_section_id(regular_part_base), None);
        for offset in 0..P::NUM_BUILT_IN_REGULAR_SECTIONS {
            let section_id = regular_section_base.offset(offset);
            for part_id in section_id.parts::<P>() {
                let alignment = output_sections.part_alignment::<P>(part_id);
                assert_eq!(
                    part_id.output_section_id::<P>(),
                    section_id,
                    "regular-part round trip failed for {}",
                    std::any::type_name::<P>()
                );
                assert_eq!(
                    section_id.part_id_with_alignment::<P>(alignment),
                    part_id,
                    "regular-part alignment conversion failed for {}",
                    std::any::type_name::<P>()
                );
            }
        }

        assert_eq!(
            P::built_in_section_details().len(),
            output_section_id::num_built_in_sections::<P>(),
            "built-in section definitions don't cover the ID range for {}",
            std::any::type_name::<P>()
        );
    }

    #[test]
    fn test_platform_part_id_invariants() {
        check_platform_part_ids::<crate::elf::Elf64>();
        check_platform_part_ids::<crate::macho::MachO>();
        check_platform_part_ids::<crate::wasm::Wasm>();
    }
}
