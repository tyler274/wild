use super::ids::*;
use super::order::*;
use super::types::*;
use crate::Result;
use crate::alignment;
use crate::alignment::Alignment;
use crate::alignment::NUM_ALIGNMENTS;
use crate::grouping::SequencedLinkerScript;
use crate::layout::EnginePlatform;
use crate::layout_rules::SectionKind;
use crate::linker_script;
use crate::linker_script::Expression;
use crate::linker_script::OnlyIf;
use crate::output_kind::OutputKind;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform::Args;
use crate::platform::Platform;
use crate::platform::SectionAttributes as _;
use crate::program_segments::ProgramSegments;
use crate::timing_phase;
use hashbrown::HashMap;
use hashbrown::HashSet;
use std::fmt::Display;

#[derive(Debug)]
pub(crate) struct OutputSections<'data, P: Platform> {
    /// The base address for our output binary.
    pub(crate) base_address: Expression<'data>,
    pub(crate) section_infos: OutputSectionMap<SectionOutputInfo<'data, P>>,

    // TODO: Consider moving this to Layout. We can't populate this until we know which output
    // sections have content, which we don't know until half way through the layout phase.
    /// Mapping from internal section IDs to output section indexes. None, if the section isn't
    /// being output.
    pub(crate) output_section_indexes: Vec<Option<u32>>,

    custom_by_identity: HashMap<SectionIdentity<'data, P>, OutputSectionId>,

    init_fini_by_priority: HashMap<(OutputSectionId, u16), OutputSectionId>,

    rosegment: bool,

    output_kind: OutputKind,

    /// BYTE/SHORT/LONG/QUAD data emitted by linker scripts, keyed by location-counter index.
    pub(crate) script_output_data: Vec<ScriptOutputData<'data>>,

    /// Where the linker-generated GNU build-id note should go when a linker script is in use.
    pub(crate) gnu_build_id_placement: GnuBuildIdPlacement,

    /// Size of the generated build-id note that was moved off the builtin section, if any.
    /// Used to redirect the epilogue layout cursor after GNU ld-style merging or discard.
    pub(crate) gnu_build_id_allocated: u64,

    /// `ONLY_IF_RO` / `ONLY_IF_RW` copies of the same output section name.
    only_if_slots: HashMap<OutputSectionId, OnlyIfSlots<'data>>,
}
impl<'data, P: Platform> OutputSections<'data, P> {
    /// Returns an iterator that emits all section IDs and their info.
    pub(crate) fn ids_with_info(
        &self,
    ) -> impl Iterator<Item = (OutputSectionId, &SectionOutputInfo<'data, P>)> {
        self.section_infos.iter()
    }

    // TODO: Experiment with adjusting the balance between dense and sparse sections. If we decide
    // not to make it dynamic, then remove this method and construct part maps more directly.
    #[allow(clippy::unused_self)]
    pub(crate) fn new_part_map<T: Default>(&self) -> OutputSectionPartMap<T> {
        OutputSectionPartMap::with_dense_size(
            P::NUM_SINGLE_PART_SECTIONS as usize
                + P::NUM_BUILT_IN_REGULAR_SECTIONS * NUM_ALIGNMENTS,
        )
    }

    pub(crate) fn new_section_map<T: Default>(&self) -> OutputSectionMap<T> {
        OutputSectionMap::with_size(self.num_sections())
    }

    pub(crate) fn new_section_map_with<T>(&self, new: impl FnMut() -> T) -> OutputSectionMap<T> {
        let mut values = Vec::new();
        values.resize_with(self.num_sections(), new);
        OutputSectionMap::from_values(values)
    }

    pub(crate) fn section_flags(&self, section_id: OutputSectionId) -> P::SectionFlags {
        self.output_info(section_id).section_attributes.flags()
    }

    /// Returns the ID of the primary output section for the supplied section ID.
    pub(crate) fn primary_output_section(&self, section_id: OutputSectionId) -> OutputSectionId {
        self.merge_target(section_id).unwrap_or(section_id)
    }

    /// Returns the ID of the section that the specified section should be merged into, if any, or
    /// None if the supplied section is itself a primary section.
    pub(crate) fn merge_target(&self, section_id: OutputSectionId) -> Option<OutputSectionId> {
        match self.output_info(section_id).kind {
            SectionKind::Primary(_) => None,
            SectionKind::Secondary(primary_id) => Some(primary_id),
        }
    }

    /// Returns whether we should include the specified section in a program segment with the
    /// supplied properties.
    pub(crate) fn should_include_in_segment(
        &self,
        section_id: OutputSectionId,
        segment_def: P::ProgramSegmentDef,
    ) -> bool
    where
        P: EnginePlatform,
    {
        let info = self.output_info(section_id);
        P::program_segment_should_include_section(segment_def, info, section_id, self.rosegment)
    }
}
impl<'data, P: Platform> OutputSections<'data, P> {
    pub(crate) fn secondary_order(&self, id: OutputSectionId) -> Option<SecondaryOrder> {
        self.section_infos.get(id).secondary_order
    }
    pub(crate) fn add_sections(
        &mut self,
        custom_sections: &[CustomSectionDetails<'data, P>],
        section_part_ids: &mut [PartId],
        args: &P::Args,
    ) {
        for custom in custom_sections {
            let location = args
                .start_address_for_section(custom.identity.section_name())
                .map(linker_script::Expression::Number);
            let location_info = location.map(|loc| SectionLocationInfo {
                location_counters: (0, 0),
                location: Some(loc),
                at_location: None,
                at_region: None,
                is_top_level: true,
                overlay: None,
            });
            let section_id = self.add_named_section(
                custom.identity,
                custom.alignment,
                None,
                location_info.as_ref(),
                None,
                Vec::new(),
                None,
            );

            let part_id = if section_id.is_regular::<P>() {
                section_id.part_id_with_alignment::<P>(custom.alignment)
            } else {
                section_id.base_part_id::<P>()
            };
            section_part_ids[custom.index.0] = part_id;
        }
    }

    /// Applies `--section-start` / `-Ttext` / `-Tdata` / `-Tbss` overrides to the built-in
    /// sections `.text`, `.data`, and `.bss`. Must be called after `with_base_address` and before
    /// the layout phase reads `section_info.location`.
    pub(crate) fn apply_section_start_overrides(&mut self, args: &P::Args) {
        // TODO: The names here are definitely ELF-specific. Look at moving this code.
        for (section_id, name) in [
            (P::TEXT_SECTION_ID, SectionName(b".text")),
            (P::DATA_SECTION_ID, SectionName(b".data")),
            (P::BSS_SECTION_ID, SectionName(b".bss")),
        ] {
            let Some(section_id) = section_id else {
                continue;
            };
            if let Some(address) = args.start_address_for_section(name) {
                let info = self.section_infos.get_mut(section_id);
                if let Some(ref mut loc_info) = info.location_info {
                    loc_info.location = Some(linker_script::Expression::Number(address));
                } else {
                    info.location_info = Some(SectionLocationInfo {
                        location_counters: (0, 0),
                        location: Some(linker_script::Expression::Number(address)),
                        at_location: None,
                        at_region: None,
                        is_top_level: true,
                        overlay: None,
                    });
                }
            }
        }
    }

    pub(crate) fn add_named_section(
        &mut self,
        identity: SectionIdentity<'data, P>,
        min_alignment: Alignment,
        region_name: Option<&'data [u8]>,
        location_info: Option<&SectionLocationInfo<'data>>,
        fill: Option<[u8; 4]>,
        phdrs: Vec<&'data [u8]>,
        attributes: Option<&linker_script::SectionAttributes>,
    ) -> OutputSectionId {
        let mut resolved_id = None;
        if !self.output_kind.is_partial_link() {
            if let Some(builtin_id) = (0..regular_section_base::<P>().as_usize())
                .map(OutputSectionId::from_usize)
                .find(|&bid| self.identity(bid) == Some(identity))
            {
                resolved_id = Some(builtin_id);
            } else if let Some(comment_id) = P::COMMENT_SECTION_ID
                && self.identity(comment_id) == Some(identity)
            {
                // `.comment` is a regular built-in. Script `.comment` must reuse it so
                // linker identity and `*(.comment)` share one section (GNU default script).
                resolved_id = Some(comment_id);
            }
        }

        let output_id = match self.custom_by_identity.entry(identity) {
            hashbrown::hash_map::Entry::Occupied(e) => *e.get(),
            hashbrown::hash_map::Entry::Vacant(e) => {
                if let Some(builtin_id) = resolved_id {
                    *e.insert(builtin_id)
                } else {
                    let new_id = self.section_infos.add_new(SectionOutputInfo {
                        kind: SectionKind::Primary(identity),
                        section_attributes: attributes
                            .map(|attr| P::apply_linker_script_attributes(attr, Default::default()))
                            .unwrap_or_default(),
                        min_alignment,
                        location_info: location_info.cloned(),
                        secondary_order: None,
                        region_name,
                        fill,
                        phdrs,
                        input_order: false,
                    });
                    return *e.insert(new_id);
                }
            }
        };

        let info = self.section_infos.get_mut(output_id);
        info.min_alignment = info.min_alignment.max(min_alignment);
        info.region_name = region_name.or(info.region_name);
        if location_info.is_some() {
            info.location_info = location_info.cloned();
        }
        info.fill = fill.or(info.fill);
        if !phdrs.is_empty() {
            info.phdrs = phdrs;
        }
        info.section_attributes = attributes.map_or(info.section_attributes, |attr| {
            P::apply_linker_script_attributes(attr, info.section_attributes)
        });

        output_id
    }

    pub(crate) fn add_secondary_section(
        &mut self,
        primary_id: OutputSectionId,
        min_alignment: Alignment,
        secondary_order: Option<SecondaryOrder>,
        location_info: Option<SectionLocationInfo<'data>>,
        input_order: bool,
    ) -> OutputSectionId {
        let primary_info = self.section_infos.get(primary_id);
        let section_attributes = primary_info.section_attributes;
        let location_info = location_info.or_else(|| primary_info.location_info.clone());
        self.section_infos.add_new(SectionOutputInfo {
            kind: SectionKind::Secondary(primary_id),
            section_attributes,
            min_alignment,
            location_info,
            secondary_order,
            region_name: primary_info.region_name,
            fill: primary_info.fill,
            phdrs: Vec::new(),
            input_order,
        })
    }

    pub(crate) fn set_input_order(&mut self, sid: OutputSectionId, input_order: bool) {
        self.section_infos.get_mut(sid).input_order = input_order;
    }

    pub(crate) fn uses_input_order(&self, sid: OutputSectionId) -> bool {
        self.section_infos.get(sid).input_order
    }

    pub(crate) fn with_base_address(base_address: u64, output_kind: OutputKind) -> Self
    where
        P: EnginePlatform,
    {
        let section_infos = P::built_in_section_infos();
        let base_address = Expression::Number(base_address);

        Self {
            section_infos: OutputSectionMap::from_values(section_infos),
            base_address,
            custom_by_identity: HashMap::new(),
            output_section_indexes: Default::default(),
            init_fini_by_priority: HashMap::new(),
            rosegment: true,
            output_kind,
            script_output_data: Vec::new(),
            gnu_build_id_placement: GnuBuildIdPlacement::Builtin,
            gnu_build_id_allocated: 0,
            only_if_slots: HashMap::new(),
        }
    }

    /// Part that holds the generated GNU build-id note, if it is being emitted.
    pub(crate) fn gnu_build_id_dest_part(&self) -> Option<PartId> {
        let builtin = P::NOTE_GNU_BUILD_ID_SECTION_ID?;
        match self.gnu_build_id_placement {
            GnuBuildIdPlacement::Discard => None,
            GnuBuildIdPlacement::Builtin => P::single_part_id(builtin),
            GnuBuildIdPlacement::Merge(target) => {
                if target == builtin || !target.is_regular::<P>() {
                    P::single_part_id(builtin)
                } else {
                    Some(target.part_id_with_alignment::<P>(alignment::NOTE_GNU_BUILD_ID))
                }
            }
        }
    }

    pub(crate) fn set_rosegment(&mut self, rosegment: bool) {
        self.rosegment = rosegment;
    }

    pub(crate) fn record_only_if(
        &mut self,
        id: OutputSectionId,
        only_if: OnlyIf,
        order_index: usize,
        location_info: SectionLocationInfo<'data>,
        phdrs: Vec<&'data [u8]>,
    ) {
        *self.only_if_slots.entry(id).or_default().slot_mut(only_if) = Some(OnlyIfPlacement {
            order_index,
            location_info,
            phdrs,
        });
    }

    /// GNU: if any matching input is writable, the `ONLY_IF_RW` copy is used for
    /// every matching input; otherwise the `ONLY_IF_RO` copy is used.
    pub(crate) fn apply_only_if_choice(&mut self, writable_sections: &HashSet<OutputSectionId>) {
        let ids: Vec<OutputSectionId> = self.only_if_slots.keys().copied().collect();
        for id in ids {
            let prefer_rw = writable_sections.contains(&id)
                && self
                    .only_if_slots
                    .get(&id)
                    .is_some_and(|slots| slots.rw.is_some());
            if let Some(slots) = self.only_if_slots.get_mut(&id) {
                slots.prefer_rw = prefer_rw;
            }
            if let Some(placement) = self
                .only_if_slots
                .get(&id)
                .and_then(|slots| slots.chosen().cloned())
            {
                let info = self.section_infos.get_mut(id);
                info.location_info = Some(placement.location_info);
                if !placement.phdrs.is_empty() {
                    info.phdrs = placement.phdrs;
                }
            }
        }
    }

    pub(crate) fn should_emit_only_if_order_slot(
        &self,
        id: OutputSectionId,
        order_index: usize,
    ) -> bool {
        let Some(slots) = self.only_if_slots.get(&id) else {
            return true;
        };
        slots
            .chosen()
            .is_none_or(|placement| placement.order_index == order_index)
    }

    pub(crate) fn bump_min_alignment(&mut self, sid: OutputSectionId, a: Alignment) {
        let info = self.section_infos.get_mut(sid);
        info.min_alignment = core::cmp::max(info.min_alignment, a);
    }

    pub(crate) fn get_or_create_init_fini_secondary(
        &mut self,
        primary: OutputSectionId,
        priority: u16,
        min_alignment: Alignment,
    ) -> OutputSectionId {
        let key = (primary, priority);
        if let Some(&sid) = self.init_fini_by_priority.get(&key) {
            self.bump_min_alignment(sid, min_alignment);
            return sid;
        }

        let sid = self.add_secondary_section(
            primary,
            min_alignment,
            Some(SecondaryOrder::InitFini { priority }),
            None,
            false,
        );

        self.init_fini_by_priority.insert(key, sid);
        sid
    }

    pub(crate) fn output_order(
        &self,
        output_kind: OutputKind,
        linker_scripts: &[&SequencedLinkerScript<'data, P>],
        location_counters: &[crate::layout_rules::LocationCounter<'data>],
    ) -> Result<(OutputOrder<'data>, ProgramSegments<P::ProgramSegmentDef>)>
    where
        P: EnginePlatform,
    {
        timing_phase!("Compute output order");

        let has_custom_phdrs = linker_scripts
            .iter()
            .any(|s| !s.parsed.program_headers.is_empty());

        let mut custom = CustomSectionIds {
            place_after_similar: !linker_scripts.is_empty(),
            ..Default::default()
        };

        let mut secondary: OutputSectionMap<Vec<OutputSectionId>> = self.new_section_map();

        self.section_infos.for_each(|id, info| {
            if let SectionKind::Secondary(primary) = info.kind {
                secondary.get_mut(primary).push(id);
                return;
            }

            if !id.is_regular::<P>() && P::single_part_id(id).is_none() {
                return;
            }

            if has_custom_phdrs {
                if !info.phdrs.is_empty() {
                    return;
                }

                if id == FILE_HEADER || P::CUSTOM_PHDR_EXCLUDED_SECTION_IDS.contains(&id) {
                    return;
                } else if id.as_usize() < num_built_in_sections::<P>()
                    && let Some(identity) = self.identity(id)
                    && self.custom_identity_to_id(identity).is_some()
                {
                    return;
                }
            } else if !id.is_custom::<P>() {
                return;
            }

            let attr = info.section_attributes;
            if attr.is_executable() {
                custom.exec.push(id);
            } else if attr.is_tls() {
                if attr.is_no_bits() {
                    custom.tbss.push(id);
                } else {
                    custom.tdata.push(id);
                }
            } else if !attr.is_writable() {
                if attr.is_alloc() {
                    custom.ro.push(id);
                } else {
                    custom.nonalloc.push(id);
                }
            } else if attr.is_no_bits() {
                custom.bss.push(id);
            } else {
                custom.data.push(id);
            }
        });

        let script_section_order: Vec<OutputSectionId> = linker_scripts
            .iter()
            .flat_map(|script| {
                script
                    .parsed
                    .ordered_sections
                    .iter()
                    .copied()
                    .enumerate()
                    .filter_map(|(index, id)| {
                        self.should_emit_only_if_order_slot(id, index).then_some(id)
                    })
            })
            .collect();

        let (mut output_order, program_segments) = if has_custom_phdrs {
            P::build_custom_output_order_and_program_segments(
                &custom,
                output_kind,
                self,
                &secondary,
                linker_scripts,
                location_counters,
            )?
        } else {
            P::build_output_order_and_program_segments(
                &custom,
                output_kind,
                self,
                &secondary,
                location_counters,
            )
        };
        output_order.set_script_section_order(script_section_order);
        Ok((output_order, program_segments))
    }

    #[must_use]
    pub(crate) fn num_sections(&self) -> usize {
        self.section_infos.len()
    }

    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn num_regular_sections(&self) -> usize {
        self.section_infos.len() - regular_section_base::<P>().as_usize()
    }

    pub(crate) fn has_data_in_file(&self, section_id: OutputSectionId) -> bool {
        let attributes = self.output_info(section_id).section_attributes;
        !attributes.is_no_bits()
    }

    pub(crate) fn output_info(&self, id: OutputSectionId) -> &SectionOutputInfo<'data, P> {
        self.section_infos.get(id)
    }

    /// Returns the output index of the built-in-section `id` or None if the section isn't being
    /// output.
    pub(crate) fn output_index_of_section(&self, id: OutputSectionId) -> Option<u32> {
        self.output_section_indexes
            .get(id.as_usize())
            .copied()
            .flatten()
    }

    pub(crate) fn output_index_of_nearest_section(&self, id: OutputSectionId) -> Option<u32> {
        self.output_index_of_section(id).or_else(|| {
            self.previous_emitted_section_id(id)
                .and_then(|prev| self.output_index_of_section(prev))
        })
    }

    /// Previous emitted section in output-section-id order, skipping the file-header
    /// placeholder that GNU ld does not treat as a real section.
    pub(crate) fn previous_emitted_section_id(
        &self,
        id: OutputSectionId,
    ) -> Option<OutputSectionId> {
        self.output_section_indexes[..id.as_usize()]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, idx)| idx.and_then(|_| Self::emitted_neighbor_id(i)))
    }

    /// Next emitted section in output-section-id order.
    pub(crate) fn following_emitted_section_id(
        &self,
        id: OutputSectionId,
    ) -> Option<OutputSectionId> {
        self.output_section_indexes
            .iter()
            .enumerate()
            .skip(id.as_usize() + 1)
            .find_map(|(i, idx)| idx.and_then(|_| Self::emitted_neighbor_id(i)))
    }

    fn emitted_neighbor_id(index: usize) -> Option<OutputSectionId> {
        let id = OutputSectionId::from_usize(index);
        if id == FILE_HEADER { None } else { Some(id) }
    }

    /// Returns whether we're going to emit the specified section.
    pub(crate) fn will_emit_section(&self, id: OutputSectionId) -> bool {
        self.output_index_of_section(id).is_some()
    }

    pub(crate) fn identity(
        &self,
        section_id: OutputSectionId,
    ) -> Option<SectionIdentity<'data, P>> {
        match self.output_info(section_id).kind {
            SectionKind::Primary(identity) => Some(identity),
            SectionKind::Secondary(_) => None,
        }
    }

    pub(crate) fn name(&self, section_id: OutputSectionId) -> Option<SectionName<'data>> {
        self.identity(section_id)
            .map(|identity| identity.section_name())
    }

    pub(crate) fn display_name(&self, section_id: OutputSectionId) -> String {
        match self.output_info(section_id).kind {
            SectionKind::Primary(identity) => format!("`{identity}`"),
            SectionKind::Secondary(primary_id) => {
                format!("{} (secondary)", self.display_name(primary_id))
            }
        }
    }

    pub(crate) fn part_debug(&self, part_id: PartId) -> String {
        let alignment = part_id.alignment(self);
        format!(
            "{} align={alignment}",
            self.section_debug(part_id.output_section_id::<P>())
        )
    }

    pub(crate) fn section_debug(&self, section_id: OutputSectionId) -> String {
        let merge_target = self.primary_output_section(section_id);
        let merge = if merge_target == section_id {
            String::new()
        } else {
            format!(" merged into {merge_target}")
        };
        format!("{section_id}{merge} ({})", self.display_name(merge_target))
    }

    pub(crate) fn custom_identity_to_id<'a>(
        &self,
        identity: SectionIdentity<'a, P>,
    ) -> Option<OutputSectionId> {
        self.custom_by_identity.get(&identity).copied()
    }

    /// Look up a section by name across both built-in and custom sections.
    /// Returns None if the platform cannot construct an identity from the name alone or if no
    /// matching section exists.
    pub(crate) fn section_id_by_name<'a>(&self, name: SectionName<'a>) -> Option<OutputSectionId> {
        let identity = P::section_identity_from_name(name)?;
        if let Some(id) = self.custom_by_identity.get(&identity).copied() {
            return Some(id);
        }
        let mut found = None;
        self.section_infos.for_each(|id, _| {
            if found.is_none() && self.identity(id) == Some(identity) {
                found = Some(id);
            }
        });
        found
    }

    /// Returns whether the specified section should have a `STT_SECTION` symbol emitted for it.
    /// Used for relocatable output (`-r`) and for fully linked `--emit-relocs`.
    pub(crate) fn will_emit_section_symbol_for_partial_objects(
        &self,
        section_id: OutputSectionId,
    ) -> bool
    where
        P: EnginePlatform,
    {
        P::will_emit_section_symbol_for_partial_objects(self, section_id)
    }

    pub(crate) fn set_base_address(&mut self, base_address: Expression<'data>) {
        self.base_address = base_address;
    }

    #[cfg(test)]
    pub(crate) fn for_testing() -> OutputSections<'static, crate::elf::Elf64> {
        use crate::elf::Elf64;

        let output_kind =
            crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed);
        let mut output_sections = OutputSections::<Elf64>::with_base_address(0x1000, output_kind);
        let mut add_name = |name: &'static str| {
            output_sections.add_named_section(
                SectionIdentity::new(SectionName(name.as_bytes()), ()),
                crate::alignment::MIN,
                None,
                None,
                None,
                Vec::new(),
                None,
            )
        };
        add_name("ro");
        add_name("exec");
        add_name("data");
        add_name("bss");
        output_sections
    }
}
impl<P: Platform> Display for OutputSections<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.section_infos.for_each(|section_id, info| {
            let _ = writeln!(f, "{section_id}: {}", info.kind);
        });
        Ok(())
    }
}

impl<P: Platform> Display for SectionKind<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SectionKind::Primary(identity) => write!(f, "{identity}"),
            SectionKind::Secondary(primary_id) => write!(f, "Secondary to {primary_id}"),
        }
    }
}
