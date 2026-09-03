use super::ids::*;
use super::sections::OutputSections;
use super::types::*;
use crate::layout_rules::LocationCounter;
use crate::layout_rules::SectionKind;
use crate::linker_script;
use crate::output_kind::OutputKind;
use crate::output_section_map::OutputSectionMap;
use crate::parsing::SymbolLoc;
use crate::platform::Platform;
use crate::platform::ProgramSegmentDef;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionType as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use core::slice;
use hashbrown::HashMap;
use hashbrown::HashSet;
use itertools::multizip;
use std::fmt::Display;

/// Encodes the order of output sections and the start and end of each program segment. This struct
/// is intended to be used by iterating over it.
#[derive(Debug)]
pub(crate) struct OutputOrder<'data> {
    events: Vec<OrderEvent<'data>>,
    num_location_counters: usize,
    has_custom_phdrs: bool,
}

pub(crate) struct OutputOrderDisplay<'a, 'data, P: Platform> {
    order: &'a OutputOrder<'data>,
    sections: &'a OutputSections<'data, P>,
    program_segments: &'a ProgramSegments<P::ProgramSegmentDef>,
}

pub(crate) struct OutputOrderBuilder<'scope, 'data, P: Platform> {
    events: Vec<OrderEvent<'data>>,

    program_segments: ProgramSegments<P::ProgramSegmentDef>,
    segment_defs: Vec<P::ProgramSegmentDef>,

    /// Indexes correspond to elements of `segment_defs`, which is typically
    /// `PROGRAM_SEGMENT_DEFS`.
    active_segment_kinds: Vec<Option<ProgramSegmentId>>,
    active_segment_regions: Vec<Option<&'data [u8]>>,

    output_sections: &'scope OutputSections<'data, P>,
    secondary: &'scope OutputSectionMap<Vec<OutputSectionId>>,
    output_kind: OutputKind,
    has_custom_phdrs: bool,
    location_counters: &'scope [crate::layout_rules::LocationCounter<'data>],
    last_location_counter: Option<LocationCounterIndex>,
    /// Custom-PHDR `PT_LOAD` starts that must wait until this section's leading
    /// `. = ALIGN(...)` has been emitted, so the LOAD inherits the script VMA.
    pending_segment_starts: Vec<ProgramSegmentId>,
}

impl<'scope, 'data, P: Platform> OutputOrderBuilder<'scope, 'data, P> {
    pub(crate) fn new(
        segment_defs: Vec<P::ProgramSegmentDef>,
        output_kind: OutputKind,
        output_sections: &'scope OutputSections<'data, P>,
        secondary: &'scope OutputSectionMap<Vec<OutputSectionId>>,
        has_custom_phdrs: bool,
        location_counters: &'scope [crate::layout_rules::LocationCounter<'data>],
    ) -> Self {
        let segment_defs_count = segment_defs.len();
        Self {
            events: Vec::new(),
            program_segments: ProgramSegments::empty(has_custom_phdrs),
            segment_defs,
            output_sections,
            active_segment_kinds: vec![None; segment_defs_count],
            active_segment_regions: vec![None; segment_defs_count],
            secondary,
            output_kind,
            has_custom_phdrs,
            location_counters,
            last_location_counter: location_counters.last().map(|_| 0),
            pending_segment_starts: Vec::new(),
        }
    }

    pub(crate) fn queue_segment_start(&mut self, segment_id: ProgramSegmentId) {
        self.pending_segment_starts.push(segment_id);
    }

    fn emit_location_counters(
        &mut self,
        lc_start: LocationCounterIndex,
        lc_end: LocationCounterIndex,
    ) {
        for idx in lc_start..lc_end {
            let lc = &self.location_counters[idx];
            match lc {
                LocationCounter::Absolute(expr, loc) => {
                    self.events
                        .push(OrderEvent::SetLocation(expr.clone(), loc.clone(), idx));
                }
                LocationCounter::Relative(expr, loc, section_id) => {
                    let primary_id = self.output_sections.primary_output_section(*section_id);
                    self.events.push(OrderEvent::SetLocationRelative(
                        expr.clone(),
                        primary_id,
                        loc.clone(),
                        idx,
                    ));
                }
            }
        }
        self.last_location_counter = self.last_location_counter.map(|l| l.max(lc_end));
    }

    pub(crate) fn add_section(&mut self, section_id: OutputSectionId) {
        // When RELRO segment ends, also end the RW LOAD segment so that subsequent non-RELRO
        // sections go into a new LOAD segment.
        if self.should_end_current_rw_segment(section_id) {
            self.end_rw_load_segment();
        }

        let (stop, start) = self.start_stop_segments_for_section(section_id);

        for segment_id in stop {
            self.events.push(OrderEvent::SegmentEnd(segment_id));
        }

        let section_info = self.output_sections.output_info(section_id);
        debug_assert!(
            matches!(section_info.kind, SectionKind::Primary(_)),
            "Attempted to directly emit secondary section {section_id}"
        );

        // Only emit SetSectionAddress if the section has ALLOC flag, meaning it can be placed in a
        // segment. Sections without ALLOC (like custom sections before their flags are propagated)
        // will have their location handled directly in compute_layout_sections.
        if let Some(ref loc_info) = section_info.location_info
            && let Some(ref location) = loc_info.location
            && section_info.section_attributes.is_alloc()
        {
            self.events
                .push(OrderEvent::SetSectionAddress(location.clone()));
        }

        // Inter-section `. = ALIGN(...)` must run before the LOAD starts, otherwise
        // `align_load_segment_start` advances the VMA by `max-page-size` and the
        // script alignment is applied on top of that (kernel `.data` at +2MB).
        if let Some(ref loc_info) = section_info.location_info {
            let (lc_start, lc_stop) = loc_info.location_counters;
            self.emit_location_counters(lc_start, lc_stop);
        }

        for segment_id in self.pending_segment_starts.drain(..) {
            self.events.push(OrderEvent::SegmentStart(segment_id));
        }

        for segment_id in start {
            self.events.push(OrderEvent::SegmentStart(segment_id));
        }

        self.events.push(OrderEvent::Section(section_id));

        let secondaries: &Vec<OutputSectionId> = self.secondary.get(section_id);
        // stable ordering: tie-break by original index
        let mut keyed: Vec<(u16, OutputSectionId)> = secondaries
            .iter()
            .map(|&sid| {
                // default: put non-initfini after all initfini, and keep their relative order
                let key_pri = match self.output_sections.secondary_order(sid) {
                    Some(crate::output_section_id::SecondaryOrder::InitFini { priority }) => {
                        priority
                    }
                    None => u16::MAX,
                };
                (key_pri, sid)
            })
            .collect();
        keyed.sort_by_key(|(pri, _sid)| *pri);

        for (_pri, sid) in keyed {
            let sec_info = self.output_sections.output_info(sid);
            if let Some(ref loc_info) = sec_info.location_info {
                let (lc_start, lc_stop) = loc_info.location_counters;
                self.emit_location_counters(lc_start, lc_stop);
            }
            self.events.push(OrderEvent::Section(sid));
        }
    }

    /// Returns true if processing the given section will cause the RELRO segment to end.
    fn should_end_current_rw_segment(&self, section_id: OutputSectionId) -> bool {
        self.active_segment_kinds
            .iter()
            .zip(self.segment_defs.iter().copied())
            .any(|(id, def)| {
                id.is_some()
                    && def.should_cut_rw_segment_when_ending()
                    && !self
                        .output_sections
                        .should_include_in_segment(section_id, def)
            })
    }

    /// Ends the currently active RW LOAD segment, if any. This is used when the RELRO segment
    /// ends to force .data and other non-RELRO sections into a new LOAD segment.
    fn end_rw_load_segment(&mut self) {
        let rw_load_def_index = self
            .segment_defs
            .iter()
            .position(|def| def.is_loadable() && def.is_writable() && !def.is_executable());

        if let Some(def_index) = rw_load_def_index
            && let Some(segment_id) = self.active_segment_kinds[def_index].take()
        {
            self.events.push(OrderEvent::SegmentEnd(segment_id));
            self.active_segment_regions[def_index] = None;
        }
    }

    /// Returns whatever `SegmentStart` and/or `SegmentEnd` events are necessary prior to the start
    /// of `section_id`. We add segment start/stop events based on the properties of the section
    /// we're about to begin. For example, if the there's a TLS segment active, but the incoming
    /// section doesn't have the TLS flag set, then we need to end the TLS segment. Similarly, if a
    /// read-only LOAD segment is active and we're about to start a section that needs to be
    /// writable, then we'll need to end the current LOAD segment and start a new writable one.
    fn start_stop_segments_for_section(
        &mut self,
        section_id: OutputSectionId,
    ) -> (Vec<ProgramSegmentId>, Vec<ProgramSegmentId>) {
        let mut stop = Vec::new();
        let mut start = Vec::new();

        if self.has_custom_phdrs {
            return (stop, start);
        }

        if self.output_kind.is_partial_link() {
            return (start, stop);
        }

        // Secondary sections don't begin or end segments.
        if self.output_sections.merge_target(section_id).is_some() {
            return (stop, start);
        }

        let section_info = self.output_sections.output_info(section_id);
        if section_info
            .location_info
            .as_ref()
            .and_then(|info| info.location.as_ref())
            .is_some()
        {
            // If we're setting the location, then first end all active segments.
            for (id, region) in self
                .active_segment_kinds
                .iter_mut()
                .zip(&mut self.active_segment_regions)
            {
                if let Some(id) = id.take() {
                    stop.push(id);
                    *region = None;
                }
            }
        }

        let section_region = section_info.region_name;
        multizip((
            self.segment_defs.iter().copied(),
            self.active_segment_kinds.iter_mut(),
            self.active_segment_regions.iter_mut(),
        ))
        .for_each(|(segment_def, active_id, active_region)| {
            let should_be_active = self
                .output_sections
                .should_include_in_segment(section_id, segment_def);

            match (active_id.as_ref(), should_be_active) {
                // Remain inactive
                (None, false) => {}

                // Remain active
                (Some(segment_id), true) => {
                    if *active_region != section_region {
                        stop.push(*segment_id);
                        let new_segment_id = self.program_segments.add_segment(segment_def);
                        start.push(new_segment_id);
                        *active_id = Some(new_segment_id);
                        *active_region = section_region;
                    }
                }
                // Start segment
                (None, true) => {
                    let segment_id = self.program_segments.add_segment(segment_def);
                    start.push(segment_id);
                    *active_id = Some(segment_id);
                    *active_region = section_region;
                }

                // End segment
                (Some(segment_id), false) => {
                    stop.push(*segment_id);
                    *active_id = None;
                    *active_region = None;
                }
            }
        });

        (stop, start)
    }

    pub(crate) fn push_event(&mut self, event: OrderEvent<'data>) {
        self.events.push(event);
    }

    pub(crate) fn add_custom_segment(
        &mut self,
        segment_def: P::ProgramSegmentDef,
    ) -> ProgramSegmentId {
        self.program_segments.add_segment(segment_def)
    }

    pub(crate) fn get_segment_mut(&mut self, id: ProgramSegmentId) -> &mut P::ProgramSegmentDef {
        self.program_segments.segment_def_mut(id)
    }

    pub(crate) fn add_sections(&mut self, sections: &[OutputSectionId]) {
        for section in sections {
            self.add_section(*section);
        }
    }

    pub(crate) fn build(mut self) -> (OutputOrder<'data>, ProgramSegments<P::ProgramSegmentDef>) {
        for segment_id in self.pending_segment_starts.drain(..) {
            self.events.push(OrderEvent::SegmentStart(segment_id));
        }

        if let Some(lc) = self.last_location_counter {
            self.emit_location_counters(lc, self.location_counters.len());
        }

        for segment_id in self.active_segment_kinds.into_iter().flatten() {
            self.events.push(OrderEvent::SegmentEnd(segment_id));
        }

        if !self.output_kind.is_partial_link() && !self.has_custom_phdrs {
            for def in P::unconditional_segment_defs() {
                let segment_id = self.program_segments.add_segment(*def);
                self.events.push(OrderEvent::SegmentStart(segment_id));
                self.events.push(OrderEvent::SegmentEnd(segment_id));
            }
        }

        (
            OutputOrder {
                events: self.events,
                num_location_counters: self.location_counters.len(),
                has_custom_phdrs: self.has_custom_phdrs,
            },
            self.program_segments,
        )
    }
}
#[derive(Debug, Clone)]
pub(crate) enum OrderEvent<'data> {
    SegmentStart(ProgramSegmentId),
    SegmentEnd(ProgramSegmentId),
    Section(OutputSectionId),
    SetLocation(
        linker_script::Expression<'data>,
        SymbolLoc,
        LocationCounterIndex,
    ),
    SetLocationRelative(
        linker_script::Expression<'data>,
        OutputSectionId,
        SymbolLoc,
        LocationCounterIndex,
    ),
    SetSectionAddress(linker_script::Expression<'data>),
}
impl<'data, 'a> IntoIterator for &'a OutputOrder<'data> {
    type Item = OrderEvent<'data>;

    type IntoIter = std::iter::Cloned<slice::Iter<'a, OrderEvent<'data>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.iter().cloned()
    }
}

impl<'data> OutputOrder<'data> {
    pub(crate) fn num_location_counters(&self) -> usize {
        self.num_location_counters
    }

    pub(crate) fn has_custom_phdrs(&self) -> bool {
        self.has_custom_phdrs
    }

    pub(crate) fn display<'a, P: Platform>(
        &'a self,
        sections: &'a OutputSections<'data, P>,
        program_segments: &'a ProgramSegments<P::ProgramSegmentDef>,
    ) -> OutputOrderDisplay<'a, 'data, P> {
        OutputOrderDisplay {
            order: self,
            sections,
            program_segments,
        }
    }
}

/// Section-header order matching GNU ld `--emit-relocs`: each copied `SHT_REL` /
/// `SHT_RELA` header sits immediately after its target. File layout is unchanged
/// (reloc contents stay with the other non-ALLOC sections).
pub(crate) fn section_header_order<'data, P: Platform>(
    output_order: &OutputOrder<'data>,
    output_sections: &OutputSections<'data, P>,
) -> Vec<OutputSectionId> {
    let mut sections = Vec::new();
    for event in output_order {
        if let OrderEvent::Section(id) = event {
            sections.push(id);
        }
    }

    let present: HashSet<OutputSectionId> = sections.iter().copied().collect();
    let mut relocs_for_target: HashMap<OutputSectionId, Vec<OutputSectionId>> = HashMap::new();
    let mut reloc_ids: HashSet<OutputSectionId> = HashSet::new();
    for &id in &sections {
        let Some(target) = copied_reloc_target(id, output_sections) else {
            continue;
        };
        if !present.contains(&target) {
            continue;
        }
        relocs_for_target.entry(target).or_default().push(id);
        reloc_ids.insert(id);
    }

    let mut out = Vec::with_capacity(sections.len());
    for id in sections {
        if reloc_ids.contains(&id) {
            continue;
        }
        out.push(id);
        if let Some(relocs) = relocs_for_target.get(&id) {
            out.extend(relocs.iter().copied());
        }
    }
    out
}

fn copied_reloc_target<'data, P: Platform>(
    id: OutputSectionId,
    output_sections: &OutputSections<'data, P>,
) -> Option<OutputSectionId> {
    let info = output_sections.output_info(id);
    if info.section_attributes.is_alloc() {
        return None;
    }
    let ty = info.section_attributes.ty();
    if !ty.is_rela() && !ty.is_rel() {
        return None;
    }
    let name = output_sections.name(id)?.0;
    let target_name = name
        .strip_prefix(b".rela")
        .or_else(|| name.strip_prefix(b".rel"))?;
    output_sections.section_id_by_name(SectionName(target_name))
}

impl<'data, P: Platform> Display for OutputOrderDisplay<'_, 'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for event in &self.order.events {
            match event {
                OrderEvent::SegmentStart(program_segment_id) => {
                    writeln!(
                        f,
                        "START({})",
                        program_segment_id.display(self.program_segments)
                    )?;
                }
                OrderEvent::SegmentEnd(program_segment_id) => {
                    writeln!(
                        f,
                        "END({})",
                        program_segment_id.display(self.program_segments)
                    )?;
                }
                OrderEvent::Section(output_section_id) => {
                    writeln!(f, "  {}", self.sections.display_name(*output_section_id))?;
                }
                OrderEvent::SetLocation(expr, ..) => {
                    writeln!(f, "SET_LOCATION({expr:?})")?;
                }
                OrderEvent::SetLocationRelative(expr, section_id, ..) => {
                    writeln!(
                        f,
                        "SET_LOCATION_RELATIVE({expr:?}, {})",
                        self.sections.display_name(*section_id)
                    )?;
                }
                OrderEvent::SetSectionAddress(expr) => {
                    writeln!(f, "SET_SECTION_ADDRESS({expr:?})")?;
                }
            }
        }

        Ok(())
    }
}
