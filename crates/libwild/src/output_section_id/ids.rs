use crate::alignment::Alignment;
use crate::alignment::NUM_ALIGNMENTS;
use crate::output_section_id::OutputSections;
use crate::part_id::PartId;
use crate::platform::Platform;
use std::ops::Range;

/// An ID for an output section. This is used for looking up section info. It's independent of
/// section ordering.
#[derive(Clone, Copy, PartialEq, Eq, Hash, derive_more::Debug)]
#[debug("osid-{_0}")]
pub(crate) struct OutputSectionId(u32);

#[repr(u32)]
#[derive(Clone, Copy)]
pub(crate) enum CommonSinglePartSectionId {
    Unmapped,
    FileHeader,

    // Must be last.
    Count,
}

impl CommonSinglePartSectionId {
    pub(crate) const fn part_id(self) -> PartId {
        PartId::from_u32(self as u32)
    }

    pub(crate) const fn output_section_id(self) -> OutputSectionId {
        OutputSectionId::from_u32(self as u32)
    }
}

pub(crate) const NUM_COMMON_SINGLE_PART_SECTIONS: u32 = CommonSinglePartSectionId::Count as u32;

#[cfg(test)]
pub(crate) const UNMAPPED: OutputSectionId =
    CommonSinglePartSectionId::Unmapped.output_section_id();

pub(crate) const FILE_HEADER: OutputSectionId =
    CommonSinglePartSectionId::FileHeader.output_section_id();
impl OutputSectionId {
    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }

    pub(crate) const fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub(crate) const fn from_u32(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn from_usize(value: usize) -> Self {
        Self(value as u32)
    }

    pub(crate) const fn offset(self, offset: usize) -> Self {
        Self(self.0 + offset as u32)
    }

    pub(crate) fn part_id_range<P: Platform>(self) -> Range<PartId> {
        let base = self.base_part_id::<P>();
        let count = self.num_parts::<P>();
        base..base.offset(count)
    }

    pub(crate) fn num_parts<P: Platform>(self) -> usize {
        if self.0 < regular_section_base::<P>().0 {
            1
        } else {
            NUM_ALIGNMENTS
        }
    }

    pub(crate) fn parts<P: Platform>(self) -> PartIdIterator {
        PartIdIterator {
            next: self.base_part_id::<P>(),
            remaining: self.num_parts::<P>(),
        }
    }

    pub(crate) fn opt_built_in_details<P: Platform>(
        self,
    ) -> Option<&'static P::BuiltInSectionDetails> {
        P::built_in_section_details().get(self.as_usize())
    }

    pub(crate) fn min_alignment<P: Platform>(
        self,
        output_sections: &OutputSections<P>,
    ) -> Alignment {
        output_sections.section_infos.get(self).min_alignment
    }

    pub(crate) fn is_regular<P: Platform>(self) -> bool {
        self.0 >= regular_section_base::<P>().0
    }

    /// Returns the part ID in this section that has the specified alignment. Can only be called for
    /// regular sections.
    pub(crate) const fn part_id_with_alignment<P: Platform>(self, alignment: Alignment) -> PartId {
        let Some(regular_offset) = self.0.checked_sub(regular_section_base::<P>().0) else {
            panic!("part_id_with_alignment can only be called for regular sections");
        };
        PartId::from_u32(
            crate::part_id::regular_part_base::<P>().as_u32()
                + (regular_offset * NUM_ALIGNMENTS as u32)
                + NUM_ALIGNMENTS as u32
                - 1
                - alignment.exponent as u32,
        )
    }

    /// Returns the first part ID for this section.
    pub(crate) fn base_part_id<P: Platform>(self) -> PartId {
        if self.0 < regular_section_base::<P>().0 {
            P::single_part_id(self).unwrap_or_else(|| {
                panic!(
                    "platform {} has no part ID for output section {self:?}",
                    std::any::type_name::<P>()
                )
            })
        } else {
            PartId::from_u32(
                crate::part_id::regular_part_base::<P>().as_u32()
                    + (self.0 - regular_section_base::<P>().0) * NUM_ALIGNMENTS as u32,
            )
        }
    }

    /// Returns whether this section ID corresponds to a custom section as opposed to a built-in
    /// section.
    pub(crate) const fn is_custom<P: Platform>(self) -> bool {
        self.as_usize() >= num_built_in_sections::<P>()
    }
}

pub(crate) const fn regular_section_base<P: Platform>() -> OutputSectionId {
    OutputSectionId::from_u32(P::NUM_SINGLE_PART_SECTIONS)
}
impl std::fmt::Display for OutputSectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.as_usize(), f)
    }
}
pub(crate) struct PartIdIterator {
    next: PartId,
    remaining: usize,
}

impl Iterator for PartIdIterator {
    type Item = PartId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            None
        } else {
            self.remaining -= 1;
            let id = self.next;
            self.next = self.next.offset(1);
            Some(id)
        }
    }
}
pub(crate) const fn num_built_in_sections<P: Platform>() -> usize {
    regular_section_base::<P>().as_usize() + P::NUM_BUILT_IN_REGULAR_SECTIONS
}
