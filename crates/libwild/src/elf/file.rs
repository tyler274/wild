#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::gnu::*;
#[allow(unused_imports)]
use super::output::*;
use super::output_section_id;
use super::part_id;
#[allow(unused_imports)]
use super::types::*;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_kind::FileKind;
use crate::file_writer::copy_section_data;
use crate::input_data::InputBytes;
use crate::layout;
use crate::layout::DynamicSymbolDefinition;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform;
use crate::platform::Arch;
use crate::platform::FrameIndex;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Relocation;
use crate::platform::RelocationSequence;
use crate::platform::Symbol as _;
use crate::resolution::LoadedMetrics;
use linker_utils::elf::sht;
use object::LittleEndian;
use object::read::elf::CompressionHeader;
use object::read::elf::FileHeader as _;
use object::read::elf::RelocationSections;
use object::read::elf::SectionHeader as _;
use rayon::Scope;
use std::borrow::Cow;
use std::sync::atomic::Ordering;
use zerocopy::FromBytes;

impl<'data, C: ElfClass> File<'data, C> {
    pub(super) fn parse_elf_bytes(data: &'data [u8], is_dynamic: bool) -> Result<Self> {
        let header = C::FileHeader::parse(data)?;
        let endian = header.endian()?;
        let architecture = header.e_machine(endian).try_into()?;
        let sections = header.sections(endian, data)?;
        let eflags = header.e_flags(endian);

        let mut symbols = SymbolTable::<C>::default();
        let mut versym: &[Versym] = &[];
        let mut verdef = None;
        let mut verdefnum = 0;
        let mut verneed = None;

        // Find all the sections that we're interested in a single scan of the section table so
        // as to avoid multiple scans.
        for (section_index, section) in sections.enumerate() {
            match section.sh_type(endian) {
                sht::DYNSYM if is_dynamic => {
                    symbols =
                        SymbolTable::<C>::parse(endian, data, &sections, section_index, section)?;
                }
                sht::SYMTAB if !is_dynamic => {
                    symbols =
                        SymbolTable::<C>::parse(endian, data, &sections, section_index, section)?;
                }
                sht::GNU_VERSYM => {
                    versym = section.data_as_array(endian, data)?;
                }
                sht::GNU_VERDEF => {
                    verdef = section.gnu_verdef(endian, data)?;
                    verdefnum = section.sh_info(endian);
                }
                sht::GNU_VERNEED => {
                    verneed = section.gnu_verneed(endian, data)?;
                }
                _ => {}
            }
        }

        let dynamic_tag_values =
            is_dynamic.then(|| DynamicTagValues::read::<C>(&sections, data, &symbols));

        Ok(Self {
            arch: architecture,
            data,
            sections,
            symbols,
            versym,
            verdef,
            verdefnum,
            verneed,
            eflags,
            dynamic_tag_values,
        })
    }
}

impl<'data, C: ElfClass> platform::ObjectFile<'data> for File<'data, C> {
    type Platform = Elf<C>;

    fn parse(input: &InputBytes<'data>, args: &ElfArgs) -> Result<Self> {
        let is_dynamic = input.kind == FileKind::ElfDynamic;

        let file = Self::parse_bytes(input.data, is_dynamic)?;

        if file.arch != args.architecture() {
            bail!(
                "`{}` has incompatible architecture: {}, expecting {}",
                input,
                file.arch,
                args.architecture(),
            )
        }

        Ok(file)
    }

    fn parse_bytes(data: &'data [u8], is_dynamic: bool) -> Result<Self> {
        Self::parse_elf_bytes(data, is_dynamic)
    }

    fn section(&self, index: object::SectionIndex) -> Result<&SectionHeader<C>> {
        Ok(self.sections.section(index)?)
    }

    fn section_by_name(&self, name: &str) -> Option<(object::SectionIndex, &SectionHeader<C>)> {
        self.sections.section_by_name(LittleEndian, name.as_bytes())
    }

    fn section_name(&self, index: object::SectionIndex) -> Result<&'data [u8]> {
        let section = self.sections.section(index)?;
        Ok(self.sections.section_name(LittleEndian, section)?)
    }

    fn section_display_name(&self, index: object::SectionIndex) -> Cow<'data, str> {
        self.section_name(index).map_or_else(
            |_| format!("<index {}>", index.0).into(),
            String::from_utf8_lossy,
        )
    }

    fn raw_section_data(&self, section: &SectionHeader<C>) -> Result<&'data [u8]> {
        Ok(section.data(LittleEndian, self.data)?)
    }

    fn section_data(
        &self,
        section: &SectionHeader<C>,
        member: &bumpalo_herd::Member<'data>,
        loaded_metrics: &LoadedMetrics,
    ) -> Result<&'data [u8]> {
        let data = section.data(LittleEndian, self.data)?;
        loaded_metrics
            .loaded_bytes
            .fetch_add(data.len(), Ordering::Relaxed);

        if let Some((compression, _, _)) = section.compression(LittleEndian, self.data)? {
            loaded_metrics
                .loaded_compressed_bytes
                .fetch_add(data.len(), Ordering::Relaxed);
            let len = self.section_size(section)?;
            let decompressed = member.alloc_slice_fill_default(len as usize);
            decompress_into(
                compression,
                &data[size_of::<CompressionHeaderEntry<C>>()..],
                decompressed,
            )?;
            loaded_metrics
                .decompressed_bytes
                .fetch_add(decompressed.len(), Ordering::Relaxed);
            Ok(decompressed)
        } else {
            Ok(data)
        }
    }

    fn copy_section_data(&self, section: &SectionHeader<C>, out: &mut [u8]) -> Result {
        let data = section.data(LittleEndian, self.data)?;

        if let Some((compression, _, _)) = section.compression(LittleEndian, self.data)? {
            decompress_into(
                compression,
                &data[size_of::<CompressionHeaderEntry<C>>()..],
                out,
            )?;
        } else {
            copy_section_data(data, out);
        }
        Ok(())
    }

    fn section_data_cow(&self, section: &SectionHeader<C>) -> Result<Cow<'data, [u8]>> {
        let data = section.data(LittleEndian, self.data)?;

        if let Some((compression, _, _)) = section.compression(LittleEndian, self.data)? {
            let len = self.section_size(section)?;
            let mut decompressed = vec![0; len as usize];
            decompress_into(
                compression,
                &data[size_of::<CompressionHeaderEntry<C>>()..],
                &mut decompressed,
            )?;
            Ok(Cow::Owned(decompressed))
        } else {
            Ok(Cow::Borrowed(data))
        }
    }

    fn section_size(&self, section: &SectionHeader<C>) -> Result<u64> {
        Ok(section.compression(LittleEndian, self.data)?.map_or_else(
            || section.sh_size(LittleEndian).into(),
            |compression| compression.0.ch_size(LittleEndian).into(),
        ))
    }

    fn section_alignment(&self, section: &SectionHeader<C>) -> Result<u64> {
        Ok(section.compression(LittleEndian, self.data)?.map_or_else(
            || section.sh_addralign(LittleEndian).into(),
            |compression| compression.0.ch_addralign(LittleEndian).into(),
        ))
    }

    fn relocations(
        &self,
        index: object::SectionIndex,
        relocations: &RelocationSections,
    ) -> Result<RelocationList<'data, C>> {
        let Some(section_index) = relocations.get(index) else {
            return Ok(RelocationList::Rela(&[]));
        };
        let section = self.sections.section(section_index)?;
        Ok(
            if let Some((rela, _)) = section.rela(LittleEndian, self.data)? {
                RelocationList::Rela(rela)
            } else if let Some((crel, _)) = section.crel(LittleEndian, self.data)? {
                RelocationList::Crel(crel)
            } else {
                RelocationList::Rela(&[])
            },
        )
    }

    fn symbol(&self, index: object::SymbolIndex) -> Result<&SymtabEntry<C>> {
        Ok(self.symbols.symbol(index)?)
    }

    fn symbol_name(&self, symbol: &SymtabEntry<C>) -> Result<&'data [u8]> {
        Ok(self.symbols.symbol_name(LittleEndian, symbol)?)
    }

    fn symbol_section(
        &self,
        symbol: &SymtabEntry<C>,
        index: object::SymbolIndex,
    ) -> Result<Option<object::SectionIndex>> {
        Ok(self.symbols.symbol_section(LittleEndian, symbol, index)?)
    }

    fn dynamic_tags(&self) -> Result<&'data [DynamicEntry<C>]> {
        dynamic_tags::<C>(&self.sections, self.data)
    }

    fn parse_relocations(&self) -> Result<RelocationSections> {
        Ok(self
            .sections
            .relocation_sections(LittleEndian, self.symbols.section())?)
    }

    fn section_has_relocations(
        &self,
        index: object::SectionIndex,
        relocations: &RelocationSections,
    ) -> bool {
        relocations.get(index).is_some()
    }

    fn num_symbols(&self) -> usize {
        self.symbols.len()
    }

    fn is_dynamic(&self) -> bool {
        self.dynamic_tag_values.is_some()
    }

    fn dynamic_tag_values(&self) -> Option<DynamicTagValues<'data>> {
        self.dynamic_tag_values
    }

    fn symbol_version_debug(&self, symbol_index: object::SymbolIndex) -> Option<String> {
        let endian = LittleEndian;
        let versym = self.versym.get(symbol_index.0)?;
        let versym = versym.0.get(endian);
        let is_default = !versym.is_hidden();
        let symbol_version_index = versym.index();
        if let Some((verdefs, string_table_index)) = self.verdef.clone() {
            let strings = self
                .sections
                .strings(endian, self.data, string_table_index)
                .ok()?;
            for r in verdefs {
                let (verdef, aux_iterator) = r.ok()?;
                for aux in aux_iterator {
                    let aux = aux.ok()?;
                    let version_index = verdef.vd_ndx.get(endian);
                    if version_index == symbol_version_index {
                        return Some(format!(
                            "{}{}",
                            if is_default { "@@" } else { "@" },
                            String::from_utf8_lossy(aux.name(endian, strings).ok()?)
                        ));
                    }
                }
            }
        }
        if let Some((verneeds, string_table_index)) = self.verneed.clone() {
            let strings = self
                .sections
                .strings(endian, self.data, string_table_index)
                .ok()?;
            for r in verneeds {
                let (_verneed, aux_iterator) = r.ok()?;
                for aux in aux_iterator {
                    let aux = aux.ok()?;
                    let version_index = aux.vna_other.get(endian);
                    if version_index == symbol_version_index {
                        return Some(format!(
                            "{}{}",
                            if is_default { "@@" } else { "@" },
                            String::from_utf8_lossy(aux.name(endian, strings).ok()?)
                        ));
                    }
                }
            }
        }
        None
    }

    fn section_iter<'a>(&'a self) -> core::slice::Iter<'a, SectionHeader<C>> {
        self.sections.iter()
    }

    fn enumerate_sections(
        &self,
    ) -> impl Iterator<Item = (object::SectionIndex, &SectionHeader<C>)> {
        self.sections.enumerate()
    }

    fn get_version_names(&self) -> Result<VersionNames<'data>> {
        let endian = LittleEndian;

        let mut version_names = vec![None; self.verdefnum as usize + 1];

        // See https://refspecs.linuxfoundation.org/LSB_3.0.0/LSB-PDA/LSB-PDA.junk/symversion.html
        // for information about symbol versioning.

        if let Some((verdefs, string_table_index)) = &self.verdef {
            let strings = self
                .sections
                .strings(endian, self.data, *string_table_index)?;

            for r in verdefs.clone() {
                let (verdef, mut aux_iterator) = r?;
                // Every VERDEF entry should have at least one AUX entry. We currently only care
                // about the first one.
                let aux = aux_iterator.next()?.context("VERDEF with no AUX entry")?;
                let version_index = verdef.vd_ndx.get(endian);
                let name = aux.name(endian, strings)?;

                *version_names
                    .get_mut(usize::from(version_index))
                    .with_context(|| format!("Invalid version index {version_index}"))? =
                    Some(name);
            }
        }

        Ok(VersionNames {
            names: version_names,
        })
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &SymtabEntry<C>,
        local_index: usize,
        version_names: &VersionNames<'data>,
    ) -> Result<RawSymbolName<'data>> {
        let name_bytes = self.symbol_name(symbol)?;

        let is_default;
        let version_name;

        if let Some(versym) = self.versym.get(local_index) {
            let versym = versym.0.get(LittleEndian);
            is_default = !versym.is_hidden();
            let version_index = versym.index();
            version_name = version_names
                .names
                .get(usize::from(version_index))
                .copied()
                .flatten();
        } else {
            is_default = true;
            version_name = None;
        }

        Ok(RawSymbolName {
            name: name_bytes,
            version_name,
            is_default,
        })
    }

    fn symbols_iter(&self) -> impl Iterator<Item = &SymtabEntry<C>> {
        self.symbols.iter()
    }

    fn verneed_table(&self) -> Result<VerneedTable<'data>> {
        VerneedTable::new(self)
    }

    fn num_sections(&self) -> usize {
        self.sections.len()
    }

    fn process_gnu_note_section(
        &self,
        state: &mut ObjectLayoutStateExt<'data, C>,
        section_index: object::SectionIndex,
    ) -> Result {
        let section = self.section(section_index)?;
        let e = LittleEndian;

        let Some(notes) = object::read::elf::SectionHeader::notes(section, e, self.data)? else {
            return Ok(());
        };

        for note in notes {
            for gnu_property in note?
                .gnu_properties(e)
                .ok_or(error!("Invalid type of .note.gnu.property"))?
            {
                let gnu_property = gnu_property?;

                // Right now, skip all properties other than those with size equal to 4.
                // There are existing properties, but unused right now:
                // GNU_PROPERTY_STACK_SIZE, GNU_PROPERTY_NO_COPY_ON_PROTECTED
                // TODO: support in the future
                if gnu_property.pr_data().len() != 4 {
                    continue;
                }
                state.gnu_property_notes.push(crate::elf::GnuProperty {
                    ptype: gnu_property.pr_type(),
                    data: gnu_property.data_u32(e)?,
                });
            }
        }

        Ok(())
    }

    fn symbol_versions(&self) -> &[Versym] {
        self.versym
    }

    fn dynamic_symbol_used(
        &self,
        symbol_index: object::SymbolIndex,
        file: &mut layout::DynamicLayoutState<'data, Elf<C>>,
    ) -> Result {
        if let Some(version_index) = self.versym.get(symbol_index.0) {
            file.format_specific
                .mark_version_as_needed(*version_index)?;
        }

        Ok(())
    }

    fn finalise_sizes_dynamic(
        &self,
        lib_name: &[u8],
        state: &mut DynamicLayoutStateExt<'data, C>,
        mem_sizes: &mut OutputSectionPartMap<u64>,
    ) -> Result {
        let e = LittleEndian;
        let mut version_count = 0;

        if let Some((mut verdef_iterator, link)) = self.verdef.clone() {
            let defs = verdef_iterator.clone();

            let strings = self.sections.strings(e, self.data, link)?;
            let mut base_size = 0;
            while let Some((verdef, mut aux_iterator)) = verdef_iterator.next()? {
                let version_index = verdef.vd_ndx.get(e);

                if version_index == object::elf::VER_NDX_LOCAL {
                    bail!("Invalid version index");
                }

                let flags = verdef.vd_flags.get(e);
                let is_base = flags.contains(object::elf::VER_FLG_BASE);

                // Keep the base version and any versions that are referenced.
                let needed = is_base
                    || *state
                        .symbol_versions_needed
                        .get(usize::from(version_index - object::elf::VER_NDX_GLOBAL))
                        .context("Invalid version index")?;

                if needed {
                    // For the base version, we use the lib_name rather than the version name from
                    // the input file. This matches what GNU ld appears to do. Also, if we don't do
                    // this, then the C runtime hits an assertion failure, because it expects to be
                    // able to find a DT_NEEDED entry that matches the base name of a version.
                    let name = if is_base {
                        lib_name
                    } else {
                        // Every VERDEF entry should have at least one AUX entry.
                        let aux = aux_iterator.next()?.context("VERDEF with no AUX entry")?;
                        aux.name(e, strings)?
                    };

                    let name_size = name.len() as u64 + 1;

                    if is_base {
                        // The base version doesn't count as a version, so we don't increment
                        // version_count here. We emit it as a Verneed, whereas the actual versions
                        // are emitted as Vernaux.
                        base_size = name_size;
                    } else {
                        mem_sizes.increment(part_id::DYNSTR, name_size);
                        version_count += 1;
                    }
                }
            }

            if version_count > 0 {
                mem_sizes.increment(part_id::DYNSTR, base_size);
                mem_sizes.increment(
                    part_id::GNU_VERSION_R,
                    size_of::<crate::elf::Verneed>() as u64
                        + u64::from(version_count) * size_of::<crate::elf::Vernaux>() as u64,
                );

                state.verneed_info = Some(VerneedInfo {
                    defs,
                    string_table_index: link,
                    version_count,
                });
            }
        }

        Ok(())
    }

    fn apply_non_addressable_indexes_dynamic(
        &self,
        indexes: &mut NonAddressableIndexes,
        counts: &mut NonAddressableCounts,
        state: &mut DynamicLayoutStateExt<'_, C>,
    ) -> Result {
        state.non_addressable_indexes = *indexes;
        if let Some(info) = state.verneed_info.as_ref()
            && info.version_count > 0
        {
            counts.verneed_count += 1;
            indexes.next_gnu_version_r_index = indexes
                .next_gnu_version_r_index
                .checked_offset(info.version_count)
                .context("Symbol versions overflowed 2**16")?;
        }
        Ok(())
    }

    fn should_enforce_undefined(
        &self,
        resources: &layout::GraphResources<'data, '_, Elf<C>>,
    ) -> bool {
        let is_executable = resources.symbol_db.output_kind.is_executable();

        !resources.symbol_db. args.allow_shlib_undefined
            && is_executable
            // Like lld, our behaviour for --no-allow-shlib-undefined is to only report errors for
            // shared objects that have all their dependencies in the link. This is in contrast to
            // GNU ld which recursively loads all transitive dependencies of shared objects and
            // checks our shared object against those.
            && has_complete_deps(self, resources)
    }

    fn symbol_offset_in_section(
        &self,
        symbol: &<Self::Platform as Platform>::SymtabEntry,
        _section_index: object::SectionIndex,
    ) -> Result<u64> {
        Ok(symbol.value())
    }
}

impl<C: ElfClass> DynamicLayoutStateExt<'_, C> {
    /// Marks the specified version as needed, provided it's not a local or global version.
    pub(super) fn mark_version_as_needed(&mut self, version_index: Versym) -> Result {
        let version_index = version_index.0.get(LittleEndian).index();

        // Versions 0 and 1 are local and global. We care about the versions after that.
        if !version_index.is_special() {
            *self
                .symbol_versions_needed
                .get_mut(usize::from(version_index - object::elf::VER_NDX_GLOBAL))
                .with_context(|| format!("Invalid symbol version index {version_index}"))? = true;
        }
        Ok(())
    }
}

pub(super) fn process_eh_frame_relocations<
    'data,
    'scope,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    object: &mut layout::ObjectLayoutState<'data, Elf<C>>,
    common: &mut layout::CommonGroupState<'data, Elf<C>>,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    eh_frame_section: &'data SectionHeader<C>,
    eh_frame_section_index: object::SectionIndex,
    frame_index_offset: usize,
    data: &'data [u8],
    relocations: &R::Sequence<'data>,
    scope: &Scope<'scope>,
) -> Result<Vec<ExceptionFrame<'data, R>>> {
    const PREFIX_LEN: usize = size_of::<EhFrameEntryPrefix>();

    let mut rel_iter = relocations.rel_iter().enumerate().peekable();
    let mut offset = 0;
    let mut exception_frames = Vec::new();

    while offset + PREFIX_LEN <= data.len() {
        // Although the section data will be aligned within the object file, there's
        // no guarantee that the object is aligned within the archive to any more
        // than 2 bytes, so we can't rely on alignment here. Archives are annoying!
        // See https://www.airs.com/blog/archives/170
        let prefix =
            EhFrameEntryPrefix::read_from_bytes(&data[offset..offset + PREFIX_LEN]).unwrap();
        if prefix.length == 0 {
            offset = data.len();
            // Note, linker behaviour differs here. We match lld's behaviour, which is to stop when
            // a zero-length frame is encountered. BFD ignores the frame, but continues.
            break;
        }
        let size = size_of_val(&prefix.length) + prefix.length as usize;
        let next_offset = offset + size;

        if next_offset > data.len() {
            bail!("Invalid .eh_frame data");
        }

        if prefix.cie_id == 0 {
            // This is a CIE

            // When deduplicating CIEs, we take into consideration the bytes of the CIE and all the
            // symbols it references. If however, it references something other than a symbol, then,
            // because we're not taking that into consideration, we disallow deduplication.
            let mut eligible_for_deduplication = true;
            while let Some((_, rel)) = rel_iter.peek() {
                let rel_offset = rel.offset();
                if rel_offset >= next_offset as u64 {
                    // This relocation belongs to the next entry.
                    break;
                }

                // We currently always load all CIEs, so any relocations found in CIEs always need
                // to be processed.
                process_relocation::<C, A, <R::Sequence<'data> as RelocationSequence>::Rel>(
                    object,
                    common,
                    rel,
                    eh_frame_section,
                    output_section_id::EH_FRAME.base_part_id::<Elf<C>>(),
                    resources,
                    queue,
                    false,
                    scope,
                    &mut RelrEncoder::default(), // eh_frame relocations are never RELR-eligible
                )?;

                if rel.symbol().is_none() {
                    eligible_for_deduplication = false;
                }
                rel_iter.next();
            }

            object.format_specific.cies.push(CieAtOffset {
                offset: offset as u32,
                cie: Cie {
                    bytes: &data[offset..next_offset],
                    eligible_for_deduplication,
                },
            });
        } else {
            // This is an FDE
            let mut section_index = None;
            let rel_start_index = rel_iter.peek().map_or(0, |(i, _)| *i);
            let mut rel_end_index = 0;

            while let Some((rel_index, rel)) = rel_iter.peek() {
                let rel_offset = rel.offset();
                if rel_offset < next_offset as u64 {
                    let is_pc_begin = (rel_offset as usize - offset) == FDE_PC_BEGIN_OFFSET;

                    if is_pc_begin && let Some(index) = rel.symbol() {
                        let elf_symbol = object.object.symbol(index)?;
                        section_index = object.object.symbol_section(elf_symbol, index)?;
                    }
                    rel_end_index = rel_index + 1;
                    rel_iter.next();
                } else {
                    break;
                }
            }

            if let Some(section_index) = section_index
                && let Some(unloaded) = object.sections[section_index.0].unloaded_mut()
            {
                let frame_index =
                    FrameIndex::from_usize(frame_index_offset + exception_frames.len());

                // Update our unloaded section to point to our new frame. Our frame will then in
                // turn point to whatever the section pointed to before.
                let previous_frame_for_section = unloaded.last_frame_index.replace(frame_index);

                exception_frames.push(ExceptionFrame {
                    relocations: relocations.subsequence(rel_start_index..rel_end_index),
                    frame_size: size as u32,
                    previous_frame_for_section,
                    eh_frame_section_index,
                });
            }
        }
        offset = next_offset;
    }

    common.format_specific.exception_frame_count += exception_frames.len();

    // Allocate space for any remaining bytes in .eh_frame that aren't large enough to constitute an
    // actual entry. crtend.o has a single u32 equal to 0 as an end marker.
    let remaining = &data[offset..];
    if !is_eh_frame_terminator(remaining) {
        object.format_specific.eh_frame_size += remaining.len() as u64;
    }

    Ok(exception_frames)
}

/// Processes the exception frames for a section that we're loading.
pub(super) fn process_section_exception_frames<
    'data,
    'scope,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    object: &layout::ObjectLayoutState<'data, Elf<C>>,
    frame_index: Option<FrameIndex>,
    common: &mut layout::CommonGroupState<'data, Elf<C>>,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    scope: &Scope<'scope>,
    exception_frames: &[ExceptionFrame<'data, R>],
) -> Result<EhFrameSizes> {
    let mut num_frames = 0;
    let mut eh_frame_size = 0;
    let mut next_frame_index = frame_index;
    while let Some(frame_index) = next_frame_index {
        let frame_data = &exception_frames[frame_index.as_usize()];
        next_frame_index = frame_data.previous_frame_for_section;

        eh_frame_size += u64::from(frame_data.frame_size);

        num_frames += 1;

        // Request loading of any sections/symbols referenced by the FDEs for our
        // section.
        let eh_frame_section = object.object.section(frame_data.eh_frame_section_index)?;
        for rel in frame_data.relocations.rel_iter() {
            process_relocation::<C, A, <R::Sequence<'data> as RelocationSequence>::Rel>(
                object,
                common,
                &rel,
                eh_frame_section,
                output_section_id::EH_FRAME.base_part_id::<Elf<C>>(),
                resources,
                queue,
                true,
                scope,
                &mut RelrEncoder::default(), // eh_frame relocations are never RELR-eligible
            )?;
        }
        common.format_specific.exception_frame_relocations +=
            frame_data.relocations.num_relocations();
    }

    Ok(EhFrameSizes {
        num_frames,
        eh_frame_size,
    })
}

pub(super) fn allocate_sysv_hash<C: ElfClass>(
    state: &mut EpilogueLayoutExt,
    current_sizes: &OutputSectionPartMap<u64>,
    extra_sizes: &mut OutputSectionPartMap<u64>,
    dynamic_symbol_defs: &[DynamicSymbolDefinition<Elf<C>>],
) -> Result {
    let num_defs = dynamic_symbol_defs.len();
    if num_defs == 0 {
        return Ok(());
    }

    let bucket_count = (num_defs / 2).max(1).next_power_of_two() as u32;
    // Whereas `num_defs` above is the number of definitions, this is the number of dynamic
    // symbols, which also includes undefined dynamic symbols.
    let num_dynsym = current_sizes.get(part_id::DYNSYM) / C::SYMTAB_ENTRY_SIZE;
    let chain_count = num_dynsym
        .try_into()
        .context("Too many dynamic symbols for .hash")?;

    let sysv_hash_layout = SysvHashLayout {
        bucket_count,
        chain_count,
    };

    extra_sizes.increment(part_id::SYSV_HASH, sysv_hash_layout.byte_size()?);
    state.sysv_hash_layout = Some(sysv_hash_layout);

    Ok(())
}

/// Computes a mapping from input versions to output versions.
pub(super) fn compute_version_mapping(
    symbol_versions_needed: &[bool],
    non_addressable_indexes: NonAddressableIndexes,
) -> Vec<object::elf::VersionIndex> {
    let mut out = vec![object::elf::VER_NDX_GLOBAL; symbol_versions_needed.len()];
    let mut next_output_version = non_addressable_indexes.next_gnu_version_r_index;
    for (input_version, needed) in symbol_versions_needed.iter().enumerate() {
        if *needed {
            out[input_version] = next_output_version;
            next_output_version += 1;
        }
    }
    out
}
