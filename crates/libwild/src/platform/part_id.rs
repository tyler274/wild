use super::Platform;
use super::output_section_id::CommonSinglePartSectionId;
use super::output_section_id::OutputSectionId;
use super::output_section_id::regular_section_base;
use wild_util::alignment::NUM_ALIGNMENTS;

/// An ID for a part of an output section. Parts IDs are ordered with generated
/// single-part-per-section parts first, followed by parts that belong to multi-part sections,
/// followed by sections that are partitioned by alignment and lastly custom sections, which are
/// also partitioned by alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PartId(u32);

pub(crate) const UNMAPPED: PartId = CommonSinglePartSectionId::Unmapped.part_id();

pub(crate) const FILE_HEADER: PartId = CommonSinglePartSectionId::FileHeader.part_id();

pub(crate) const fn regular_part_base<P: Platform>() -> PartId {
    PartId::from_u32(P::NUM_SINGLE_PART_SECTIONS)
}

impl PartId {
    /// A placeholder used for custom sections before we know their actual PartId.
    pub(crate) const CUSTOM_PLACEHOLDER: PartId = PartId(u32::MAX);

    pub(crate) fn output_section_id<P: Platform>(self) -> OutputSectionId {
        if self < regular_part_base::<P>() {
            P::single_part_output_section_id(self).unwrap_or_else(|| {
                panic!(
                    "platform {} has no output section ID for part {self:?}",
                    std::any::type_name::<P>()
                )
            })
        } else {
            OutputSectionId::from_u32(
                (self.0 - regular_part_base::<P>().0) / (NUM_ALIGNMENTS as u32)
                    + regular_section_base::<P>().as_u32(),
            )
        }
    }

    pub(crate) fn from_usize(raw: usize) -> Self {
        PartId(u32::try_from(raw).expect("Part IDs overflowed 32 bits"))
    }

    pub(crate) fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }

    pub(crate) const fn offset(self, offset: usize) -> PartId {
        PartId(self.0 + offset as u32)
    }

    pub(crate) const fn from_u32(value: u32) -> PartId {
        PartId(value)
    }

    /// Returns whether we should skip adding padding after this section.
    pub(crate) fn should_pack<P: Platform>(self) -> bool {
        let section_id = self.output_section_id::<P>();
        P::PACKED_SECTION_IDS.contains(&section_id)
    }
}

#[cfg(test)]
pub(crate) fn built_in_part_ids<P: Platform>() -> impl Iterator<Item = PartId> {
    let regular_part_base = regular_part_base::<P>();
    let single_part_ids = (0..regular_part_base.0).map(PartId::from_u32);
    let regular_part_ids = (0..P::NUM_BUILT_IN_REGULAR_SECTIONS * NUM_ALIGNMENTS)
        .map(move |offset| regular_part_base.offset(offset));
    single_part_ids.chain(regular_part_ids)
}

impl std::fmt::Display for PartId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.as_usize(), f)
    }
}
