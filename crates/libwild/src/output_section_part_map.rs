use crate::alignment;
use crate::alignment::Alignment;
use crate::layout::EnginePlatform;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::part_id::PartId;
use crate::platform::Platform;
#[allow(unused_imports)]
pub(crate) use crate::platform::output_section_part_map::*;
use std::ops::Range;

impl<T: Default + PartialEq> OutputSectionPartMap<T> {
    /// Iterate through all contained T in output order, producing a new map of U from the values
    /// returned by the callback. Note, the alignment is the alignment of the PartId, but capped at
    /// the maximum alignment of the highest alignment PartId with a non-default value.
    pub(crate) fn output_order_map<U: Default, P: EnginePlatform>(
        &self,
        output_order: &OutputOrder,
        output_sections: &OutputSections<P>,
        mut cb: impl FnMut(PartId, Alignment, &T) -> U,
    ) -> OutputSectionPartMap<U> {
        let mut output = OutputSectionPartMap::with_dense_size(self.dense_len());

        for event in output_order {
            let OrderEvent::Section(section_id) = event else {
                continue;
            };

            let part_id_range = section_id.part_id_range::<P>();
            let max_alignment = self.max_alignment(part_id_range.clone(), output_sections);

            for (part_id, input) in self.in_range(part_id_range) {
                let alignment = output_sections
                    .part_alignment::<P>(part_id)
                    .min(max_alignment);
                *output.get_mut(part_id) = cb(part_id, alignment, input);
            }
        }

        output
    }

    /// Returns the maximum alignment for any part with a non-default value starting from
    /// `base_part_id` for the next `count` parts. The returned value will not be any less than the
    /// minimum alignment for the section.
    pub(crate) fn max_alignment<P: EnginePlatform>(
        &self,
        range: Range<PartId>,
        output_sections: &OutputSections<P>,
    ) -> Alignment {
        self.in_range(range.clone())
            .find(|(_, value)| **value != T::default())
            .map_or(alignment::MIN, |(part_id, _)| {
                output_sections.part_alignment::<P>(part_id)
            })
            .max(output_sections.min_alignment(range.start.output_section_id::<P>()))
    }
}

#[test]
fn test_merge_parts() {
    use crate::elf::Elf64;

    let output_sections = crate::output_section_id::OutputSections::<Elf64>::for_testing();
    let (output_order, _program_segments) = output_sections
        .output_order(
            crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed),
            &[],
            &[],
        )
        .unwrap();

    let mut part_map = output_sections.new_part_map::<u32>();
    for (section_id, _) in output_sections.ids_with_info() {
        if section_id.is_custom::<Elf64>() {
            let _ =
                part_map.get_mut(section_id.part_id_with_alignment::<Elf64>(crate::alignment::MIN));
        }
    }

    let mut expected_sum_of_sums = 0;
    let all_1 = part_map.output_order_map(&output_order, &output_sections, |_, _, _| {
        expected_sum_of_sums += 1;
        1
    });

    let mut num_sections_with_all_alignments = 0;

    let mut sum_of_1s = output_sections.new_section_map::<u32>();
    sum_of_1s.for_each_mut(|section_id, sum| {
        if !section_id.is_regular::<Elf64>()
            && <Elf64 as crate::platform::Platform>::single_part_id(section_id).is_none()
        {
            return;
        }
        let range = section_id.part_id_range::<Elf64>();
        *sum = all_1.values_in_range(range).sum();
    });

    let mut sum_of_sums = 0;
    sum_of_1s.for_each(|section_id, sum| {
        sum_of_sums += *sum;
        if *sum == crate::alignment::NUM_ALIGNMENTS as u32 {
            num_sections_with_all_alignments += 1;
        }

        let unsupported_single_part = !section_id.is_regular::<Elf64>()
            && <Elf64 as crate::platform::Platform>::single_part_id(section_id).is_none();

        let expected =
            if section_id == crate::output_section_id::UNMAPPED || unsupported_single_part {
                0
            } else if section_id.is_custom::<Elf64>() {
                1
            } else if section_id.is_regular::<Elf64>() {
                crate::alignment::NUM_ALIGNMENTS as u32
            } else {
                1
            };

        assert_eq!(*sum, expected, "Unexpected sum for section {section_id:?}");
    });
    assert_eq!(
        <Elf64 as Platform>::NUM_BUILT_IN_REGULAR_SECTIONS,
        num_sections_with_all_alignments
    );
    assert_eq!(sum_of_sums, expected_sum_of_sums);

    let mut headers_only = output_sections.new_part_map::<u32>();
    *headers_only.get_mut(crate::part_id::FILE_HEADER) += 42;

    let mut merged = output_sections.new_section_map::<u32>();
    merged.for_each_mut(|section_id, sum| {
        if !section_id.is_regular::<Elf64>()
            && <Elf64 as crate::platform::Platform>::single_part_id(section_id).is_none()
        {
            return;
        }
        let range = section_id.part_id_range::<Elf64>();
        *sum = headers_only.values_in_range(range).sum();
    });

    assert_eq!(*merged.get(crate::output_section_id::FILE_HEADER), 42);
    assert_eq!(*merged.get(crate::elf::output_section_id::TEXT), 0);
    assert_eq!(*merged.get(crate::elf::output_section_id::BSS), 0);
}

#[test]
fn test_mut_with_map() {
    let output_sections =
        crate::output_section_id::OutputSections::<crate::elf::Elf64>::for_testing();
    let mut input1 = output_sections.new_part_map::<u32>().map(|_, _| 1);
    let input2 = output_sections.new_part_map::<u32>().map(|_, _| 2);
    let expected = output_sections.new_part_map::<u32>().map(|_, _| 3);
    input1.mut_with_map(&input2, |a, b| *a += *b);
    assert_eq!(input1, expected);
}

#[test]
fn test_merge() {
    let output_sections =
        crate::output_section_id::OutputSections::<crate::elf::Elf64>::for_testing();
    let mut input1 = output_sections.new_part_map::<u32>().map(|_, _| 1);
    let input2 = output_sections.new_part_map::<u32>().map(|_, _| 2);
    let expected = output_sections.new_part_map::<u32>().map(|_, _| 3);
    input1.merge(&input2);
    assert_eq!(input1, expected);
}

/// output_order_map and `OutputSections::sections_and_segments_events` used to each independently
/// define the output order. This test made sure that they were consistent. Now the former uses the
/// latter, so this test is less important. It's kept for the time being anyway.
#[test]
fn test_output_order_map_consistent() {
    use crate::elf::Elf64;
    use itertools::Itertools;

    let output_sections =
        crate::output_section_id::OutputSections::<crate::elf::Elf64>::for_testing();
    let (output_order, _program_segments) = output_sections
        .output_order(
            crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed),
            &[],
            &[],
        )
        .unwrap();
    let mut part_map = output_sections.new_part_map::<u32>();

    let custom_sections = output_sections
        .ids_with_info()
        .map(|(section_id, _)| section_id)
        .filter(|section_id| section_id.is_custom::<Elf64>())
        .collect_vec();

    for section_id in custom_sections.into_iter().rev() {
        let _ = part_map.get_mut(section_id.part_id_with_alignment::<Elf64>(crate::alignment::MIN));
    }

    // First, make sure that all our built-in part-ids are here. If they're not, we'd fail anyway,
    // but we can give a much better failure message if we check first.
    let mut missing: hashbrown::HashSet<PartId> =
        crate::part_id::built_in_part_ids::<Elf64>().collect();
    part_map.map(|part_id, _| {
        missing.remove(&part_id);
    });
    let missing = missing.into_iter().sorted().collect_vec();
    assert!(
        missing.is_empty(),
        "Built-in sections missing from output_order_map: {}",
        missing
            .iter()
            .map(|id| format!(
                "{id} (in {})",
                output_sections.display_name(id.output_section_id::<Elf64>())
            ))
            .collect_vec()
            .join(", ")
    );

    let mut ordering_a = Vec::new();
    part_map.output_order_map(&output_order, &output_sections, |part_id, _, _| {
        let section_id = part_id.output_section_id::<Elf64>();
        if ordering_a.last() != Some(&section_id.as_usize()) {
            ordering_a.push(section_id.as_usize());
        }
    });
    let ordering_b = output_order
        .into_iter()
        .filter_map(|event| {
            if let OrderEvent::Section(id) = event {
                Some(id.as_usize())
            } else {
                None
            }
        })
        .collect_vec();

    assert_eq!(ordering_a, ordering_b);
}

#[test]
fn test_output_order_map() {
    use crate::elf::Elf64;
    use crate::elf::output_section_id;

    let output_sections = crate::output_section_id::OutputSections::<Elf64>::for_testing();
    let (output_order, _program_segments) = output_sections
        .output_order(
            crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed),
            &[],
            &[],
        )
        .unwrap();
    let mut part_map = output_sections.new_part_map::<u32>();

    const PART_ID1: PartId =
        output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::USIZE);
    *part_map.get_mut(PART_ID1) += 32;

    const PART_ID2: PartId =
        output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::MIN);
    *part_map.get_mut(PART_ID2) += 5;

    part_map.output_order_map(
        &output_order,
        &output_sections,
        |part_id, alignment, &value| match part_id {
            PART_ID1 => {
                assert_eq!(alignment, alignment::USIZE);
                assert_eq!(value, 32);
            }
            PART_ID2 => {
                assert_eq!(alignment, alignment::MIN);
                assert_eq!(value, 5);
            }
            _ => {
                if part_id.output_section_id::<Elf64>() == output_section_id::DATA {
                    assert!(
                        alignment <= alignment::USIZE,
                        "Unexpected alignment {alignment}"
                    );
                }
                assert_eq!(value, 0);
            }
        },
    );
}

#[test]
fn test_max_alignment() {
    use crate::elf::Elf64;
    use crate::elf::output_section_id;

    let output_sections = crate::output_section_id::OutputSections::<Elf64>::for_testing();
    let mut part_map = output_sections.new_part_map::<u32>();

    assert_eq!(
        part_map.max_alignment(
            output_section_id::DATA.part_id_range::<Elf64>(),
            &output_sections,
        ),
        alignment::MIN
    );

    const PART_ID1: PartId =
        output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::USIZE);
    *part_map.get_mut(PART_ID1) += 32;

    const PART_ID2: PartId =
        output_section_id::DATA.part_id_with_alignment::<Elf64>(alignment::MIN);
    *part_map.get_mut(PART_ID2) += 5;

    assert_eq!(
        part_map.max_alignment(
            output_section_id::DATA.part_id_range::<Elf64>(),
            &output_sections,
        ),
        alignment::USIZE
    );
}
