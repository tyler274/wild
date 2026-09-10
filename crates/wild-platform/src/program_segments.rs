use super::ProgramSegmentDef;
use std::fmt::Display;

#[derive(Default, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Hash, Debug)]
pub struct ProgramSegmentId(u8);

#[derive(Debug)]
pub struct ProgramSegments<T: ProgramSegmentDef> {
    program_segment_details: Vec<T>,
    has_custom_phdrs: bool,
    at_lmas: Vec<Option<u64>>,
}

impl<T: ProgramSegmentDef> ProgramSegments<T> {
    pub fn empty(has_custom_phdrs: bool) -> ProgramSegments<T> {
        Self {
            program_segment_details: Vec::new(),
            has_custom_phdrs,
            at_lmas: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.program_segment_details.len()
    }

    pub fn segment_def(&self, segment_id: ProgramSegmentId) -> &T {
        &self.program_segment_details[segment_id.as_usize()]
    }

    pub fn segment_def_mut(&mut self, segment_id: ProgramSegmentId) -> &mut T {
        &mut self.program_segment_details[segment_id.as_usize()]
    }

    pub fn add_segment(&mut self, segment_def: T) -> ProgramSegmentId {
        let id = ProgramSegmentId::new(self.program_segment_details.len());
        self.program_segment_details.push(segment_def);
        self.at_lmas.push(None);
        id
    }

    pub fn set_at_lma(&mut self, segment_id: ProgramSegmentId, at_lma: u64) {
        self.at_lmas[segment_id.as_usize()] = Some(at_lma);
    }

    pub fn at_lma(&self, segment_id: ProgramSegmentId) -> Option<u64> {
        self.at_lmas.get(segment_id.as_usize()).copied().flatten()
    }

    pub fn is_load_segment(&self, segment_id: ProgramSegmentId) -> bool {
        self.segment_def(segment_id).is_loadable()
    }

    pub fn is_stack_segment(&self, segment_id: ProgramSegmentId) -> bool {
        self.segment_def(segment_id).is_stack()
    }

    pub fn is_tls_segment(&self, segment_id: ProgramSegmentId) -> bool {
        self.segment_def(segment_id).is_tls()
    }

    /// Returns a tuple that can be used for sorting the order of segments in the program headers
    /// table.
    pub fn order_key(&self, segment_id: ProgramSegmentId, mem_start: u64) -> (usize, u64) {
        let def = self.segment_def(segment_id);

        (def.order_key(), mem_start)
    }

    pub fn iter(&self) -> impl Iterator<Item = T> {
        self.program_segment_details.iter().copied()
    }

    pub fn has_custom_phdrs(&self) -> bool {
        self.has_custom_phdrs
    }
}

impl ProgramSegmentId {
    pub fn as_usize(self) -> usize {
        self.0.into()
    }

    pub fn new(segment_id: usize) -> Self {
        Self(
            segment_id
                .try_into()
                .expect("Tried to create a ProgramSegmentId >= 256"),
        )
    }

    pub fn display<T: ProgramSegmentDef>(
        self,
        program_segments: &ProgramSegments<T>,
    ) -> impl Display {
        program_segments.program_segment_details[self.0 as usize]
    }
}

impl<'a, T: ProgramSegmentDef> IntoIterator for &'a ProgramSegments<T> {
    type Item = &'a T;

    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.program_segment_details.iter()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SegmentEntry {
    pub id: ProgramSegmentId,
    pub ptype: u32,
    pub flags: u32,
    pub has_explicit_flags: bool,
    pub is_emitted: bool,
    pub has_filehdr: bool,
    pub has_phdrs: bool,
    pub at_lma: Option<u64>,
}
