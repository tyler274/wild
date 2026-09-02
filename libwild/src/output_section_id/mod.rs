//! Section and part IDs are platform-specific. These instructions apply to all platforms.
//!
//! Instructions for adding a new generated, single-part output section:
//!
//! * Add a variant to the platform's `SinglePartSectionId` enum.
//! * Define constants derived from the variant in the platform's `part_id` and `output_section_id`
//!   modules.
//! * Add the section definition info to `SECTION_DEFINITIONS`.
//! * Insert the new section into the output order in `sections_and_segments_events`. The position
//!   needs to be consistent with the access flags on the section. e.g. if the section is read-only
//!   data, it should go between the start and end of the read-only segment.
//!
//! Adding a new alignment-based (regular) section is similar to the above, but add it to the
//! platform's `RegularSectionId` enum and only define an `OutputSectionId` constant. Insert it
//! later in `SECTION_DEFINITIONS`.

pub(crate) mod ids;
pub(crate) mod order;
pub(crate) mod sections;
pub(crate) mod types;

#[allow(unused_imports)]
pub(crate) use ids::*;
#[allow(unused_imports)]
pub(crate) use order::*;
#[allow(unused_imports)]
pub(crate) use sections::*;
#[allow(unused_imports)]
pub(crate) use types::*;
