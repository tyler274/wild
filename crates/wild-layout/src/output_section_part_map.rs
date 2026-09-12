use crate::EnginePlatform;
use crate::output_section_id::{OrderEvent, OutputOrder, OutputSections};
use crate::part_id::PartId;
use std::ops::Range;
#[allow(unused_imports)]
pub use wild_platform::output_section_part_map::*;
use wild_util::alignment;
use wild_util::alignment::Alignment;

/// Iterate through all contained T in output order, producing a new map of U from the values
/// returned by the callback. Note, the alignment is the alignment of the PartId, but capped at
/// the maximum alignment of the highest alignment PartId with a non-default value.
pub fn output_order_map<T, U, P>(
    part_map: &OutputSectionPartMap<T>,
    output_order: &OutputOrder,
    output_sections: &OutputSections<P>,
    mut cb: impl FnMut(PartId, Alignment, &T) -> U,
) -> OutputSectionPartMap<U>
where
    T: Default + PartialEq,
    U: Default,
    P: EnginePlatform,
{
    let mut output = OutputSectionPartMap::with_dense_size(part_map.dense_len());

    for event in output_order {
        let OrderEvent::Section(section_id) = event else {
            continue;
        };

        let part_id_range = section_id.part_id_range::<P>();
        let max_alignment = max_alignment(part_map, part_id_range.clone(), output_sections);

        for (part_id, input) in part_map.in_range(part_id_range) {
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
pub fn max_alignment<T, P>(
    part_map: &OutputSectionPartMap<T>,
    range: Range<PartId>,
    output_sections: &OutputSections<P>,
) -> Alignment
where
    T: Default + PartialEq,
    P: EnginePlatform,
{
    part_map
        .in_range(range.clone())
        .find(|(_, value)| **value != T::default())
        .map_or(alignment::MIN, |(part_id, _)| {
            output_sections.part_alignment::<P>(part_id)
        })
        .max(output_sections.min_alignment(range.start.output_section_id::<P>()))
}
