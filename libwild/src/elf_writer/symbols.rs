use super::dynamic::*;
use super::types::*;
use crate::bail;
use crate::debug_assert_bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::GLOBAL_POINTER_SYMBOL_NAME;
use crate::elf::RawSymbolName;
use crate::elf::Verdaux;
use crate::elf::Verdef;
use crate::elf::Vernaux;
use crate::elf::Verneed;
use crate::elf::VersionDef;
use crate::elf::Versym;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::excessive_allocation;
use crate::file_writer::insufficient_allocation;
use crate::layout::DynamicLayout;
use crate::layout::FileLayout;
use crate::layout::InternalSymbols;
use crate::layout::LinkerScriptLayoutState;
use crate::layout::ObjectLayout;
use crate::layout::PreludeLayout;
use crate::layout::Resolution;
use crate::layout::SymbolCopyInfo;
use crate::linker_script::Expression;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::SymbolLoc;
use crate::platform;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::RawSymbolName as _;
use crate::platform::SectionAttributes as _;
use crate::resolution::SectionSlot;
use crate::sharding::ShardKey;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::ValueFlags;
use crate::writable_elf::WritableSymbol as _;
use linker_utils::elf::RISCV_TLS_DTV_OFFSET;
use linker_utils::elf::secnames::DYNSYM_SECTION_NAME_STR;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::LittleEndian;
use object::SymbolIndex;
use object::elf::STT_TLS;
use object::read::elf::Sym as _;
use rayon::iter::ParallelBridge as _;
use rayon::iter::ParallelIterator as _;

#[derive(Clone, Copy)]
pub(crate) enum SymbolSection {
    /// One of the SHN values.
    Raw(object::elf::SymbolSection),
    Index(u32),
}

impl From<object::elf::SymbolSection> for SymbolSection {
    fn from(value: object::elf::SymbolSection) -> Self {
        SymbolSection::Raw(value)
    }
}

#[derive(Default)]
pub(crate) struct VersionWriter<'out> {
    pub(crate) version_d: &'out mut [u8],
    pub(crate) version_r: &'out mut [u8],

    /// None if versioning is disabled, which we do if no symbols have versions.
    pub(crate) versym: Option<&'out mut [Versym]>,
}

impl<'out> VersionWriter<'out> {
    pub(crate) fn new(
        version_d: &'out mut [u8],
        version_r: &'out mut [u8],
        versym: Option<&'out mut [Versym]>,
    ) -> Self {
        Self {
            version_d,
            version_r,
            versym,
        }
    }

    pub(crate) fn set_next_symbol_version(&mut self, index: object::elf::VersionIndex) -> Result {
        if let Some(versym_table) = self.versym.as_mut() {
            let versym = versym_table
                .split_off_first_mut()
                .ok_or_else(|| insufficient_allocation(".gnu.version"))?;
            versym.0.set(LittleEndian, index.into());
        }
        Ok(())
    }

    pub(crate) fn take_bytes(&mut self, size: usize) -> Result<&'out mut [u8]> {
        self.version_r
            .split_off_mut(..size)
            .ok_or_else(|| insufficient_allocation(".gnu.version_r"))
    }

    pub(crate) fn take_verneed(&mut self) -> Result<&'out mut Verneed> {
        let bytes = self.take_bytes(size_of::<Verneed>())?;
        Ok(object::from_bytes_mut(bytes)
            .map_err(|_| error!("Incorrect .gnu.version_r alignment"))?
            .0)
    }

    pub(crate) fn take_auxes(&mut self, version_count: u16) -> Result<&'out mut [Vernaux]> {
        let bytes = self.take_bytes(size_of::<Vernaux>() * usize::from(version_count))?;
        object::slice_from_all_bytes_mut::<Vernaux>(bytes)
            .map_err(|_| error!("Invalid .gnu.version_r allocation"))
    }

    pub(crate) fn take_bytes_d(&mut self, size: usize) -> Result<&'out mut [u8]> {
        self.version_d
            .split_off_mut(..size)
            .ok_or_else(|| insufficient_allocation(".gnu.version_d"))
    }

    pub(crate) fn take_verdef(&mut self) -> Result<&'out mut Verdef> {
        let bytes = self.take_bytes_d(size_of::<Verdef>())?;
        Ok(object::from_bytes_mut::<Verdef>(bytes)
            .map_err(|_| error!("Incorrect .gnu.version_d alignment"))?
            .0)
    }

    pub(crate) fn take_verdaux(&mut self) -> Result<&'out mut Verdaux> {
        let bytes = self.take_bytes_d(size_of::<Verdaux>())?;
        Ok(object::from_bytes_mut::<Verdaux>(bytes)
            .map_err(|_| error!("Incorrect .gnu.version_d aux alignment"))?
            .0)
    }

    pub(crate) fn check_exhausted(&self, mem_sizes: &OutputSectionPartMap<u64>) -> Result {
        if let Some(versym) = self.versym.as_ref()
            && !versym.is_empty()
        {
            return Err(excessive_allocation(
                ".gnu.version",
                versym.len() as u64 * elf::GNU_VERSION_ENTRY_SIZE,
                mem_sizes.get(part_id::GNU_VERSION),
            ));
        }
        if !self.version_r.is_empty() {
            bail!(
                "Allocated too much space in .gnu.version_r. {} of {} bytes remain",
                self.version_r.len(),
                mem_sizes.get(part_id::GNU_VERSION_R)
            );
        }
        if !self.version_d.is_empty() {
            bail!(
                "Allocated too much space in .gnu.version_d. {} of {} bytes remain",
                self.version_d.len(),
                mem_sizes.get(part_id::GNU_VERSION_D)
            );
        }
        Ok(())
    }

    pub(crate) fn take_prefix(&mut self, num_symbols: usize) -> Option<&'out mut [Versym]> {
        Some(self.versym.as_mut()?.split_off_mut(..num_symbols).unwrap())
    }
}

pub(crate) struct VersionedDynsymWriter<'layout, 'out, C: ElfClass> {
    pub(crate) dynsym_writer: SymbolTableWriter<'layout, 'out, C>,
    pub(crate) versym: Option<&'out mut [Versym]>,
}

pub(crate) fn object_symbol_size<C: ElfClass>(
    sym: &elf::SymtabEntry<C>,
    sym_index: SymbolIndex,
    object: &ObjectLayout<elf::Elf<C>>,
) -> Result<u64> {
    let e = LittleEndian;
    let st_size: u64 = sym.st_size(e).into();
    if st_size == 0 {
        return Ok(0);
    }
    let Some(section_index) = object.object.symbol_section(sym, sym_index)? else {
        return Ok(st_size);
    };
    let Some(deltas) = object.section_relax_deltas.get(section_index.0) else {
        return Ok(st_size);
    };

    // Adjust symbol size for relaxation-induced byte deletions.
    let st_value: u64 = sym.st_value(e).into();
    let start_output = deltas.input_to_output_offset(st_value);
    let end_output = deltas.input_to_output_offset(st_value + st_size);
    Ok(end_output - start_output)
}

pub(crate) struct SymbolTableWriter<'layout, 'out, C: ElfClass> {
    pub(crate) local_entries: &'out mut [elf::SymtabEntry<C>],
    pub(crate) global_entries: &'out mut [elf::SymtabEntry<C>],
    pub(crate) output_sections: &'layout OutputSections<'layout, elf::Elf<C>>,
    pub(crate) strtab_writer: StrTabWriter<'out>,
    pub(crate) is_dynamic: bool,
    pub(crate) symtab_shndx_local_entries: Option<&'out mut [u32]>,
    pub(crate) symtab_shndx_global_entries: Option<&'out mut [u32]>,
}

impl<'layout, 'out, C: ElfClass> SymbolTableWriter<'layout, 'out, C> {
    pub(crate) fn new(
        start_string_offset: u32,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
        output_sections: &'layout OutputSections<'layout, elf::Elf<C>>,
    ) -> Self {
        let local_entries = slice_from_all_bytes_mut(buffers.take(part_id::SYMTAB_LOCAL));
        let global_entries = slice_from_all_bytes_mut(buffers.take(part_id::SYMTAB_GLOBAL));
        let symtab_shndx_local_entries = Some(buffers.take(part_id::SYMTAB_SHNDX_LOCAL))
            .and_then(|s| (!s.is_empty()).then(|| slice_from_all_bytes_mut(s)));
        let symtab_shndx_global_entries = Some(buffers.take(part_id::SYMTAB_SHNDX_GLOBAL))
            .and_then(|s| (!s.is_empty()).then(|| slice_from_all_bytes_mut(s)));

        let strings = buffers.take(part_id::STRTAB);
        Self {
            local_entries,
            global_entries,
            output_sections,
            strtab_writer: StrTabWriter {
                next_offset: start_string_offset,
                out: strings,
            },
            is_dynamic: false,
            symtab_shndx_local_entries,
            symtab_shndx_global_entries,
        }
    }

    pub(crate) fn new_dynamic(
        string_offset: u32,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
        output_sections: &'layout OutputSections<elf::Elf<C>>,
    ) -> Self {
        let global_entries = slice_from_all_bytes_mut(buffers.take(part_id::DYNSYM));
        let strings = slice_from_all_bytes_mut(buffers.take(part_id::DYNSTR));
        Self {
            local_entries: Default::default(),
            global_entries,
            output_sections,
            strtab_writer: StrTabWriter {
                next_offset: string_offset,
                out: strings,
            },
            is_dynamic: true,
            symtab_shndx_local_entries: None,
            symtab_shndx_global_entries: None,
        }
    }

    pub(crate) fn copy_object_symbol(
        &mut self,
        sym: &elf::SymtabEntry<C>,
        sym_index: SymbolIndex,
        symbol_id: SymbolId,
        name: &[u8],
        object: &ObjectLayout<elf::Elf<C>>,
        layout: &ElfLayout<C>,
        value: u64,
        flags: ValueFlags,
    ) -> Result {
        let e = LittleEndian;

        let section_id =
            if let Some(section_index) = object.object.symbol_section(sym, sym_index)? {
                match &object.sections[section_index.0] {
                    SectionSlot::Loaded(_)
                    | SectionSlot::Sorted(_)
                    | SectionSlot::LoadedDebugInfo(_)
                    | SectionSlot::MergeStrings(_) => object
                        .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                        .output_section_id::<elf::Elf<C>>(),
                    SectionSlot::FrameData(..) => output_section_id::EH_FRAME,
                    _ => {
                        if layout.symbol_db.is_mapping_symbol(symbol_id) {
                            return Ok(());
                        }
                        bail!(
                            "Tried to copy a symbol in a section we didn't load. {}",
                            layout.symbol_debug(symbol_id)
                        )
                    }
                }
            } else if sym.is_common(e) {
                if sym.st_type() == STT_TLS {
                    output_section_id::TBSS
                } else {
                    output_section_id::BSS
                }
            } else if sym.is_absolute(e) {
                self.copy_absolute_symbol(sym, name, flags)
                    .with_context(|| {
                        format!("Failed to absolute {}", layout.symbol_debug(symbol_id))
                    })?;
                return Ok(());
            } else {
                bail!("Attempted to output a symtab entry with an unexpected section type")
            };

        let section_id = layout.output_sections.primary_output_section(section_id);

        let entry = self.copy_symbol(sym, name, section_id, value, flags)?;

        entry.set_size(object_symbol_size(sym, sym_index, object)?)?;

        Ok(())
    }

    #[inline(always)]
    pub(crate) fn copy_symbol(
        &mut self,
        sym: &elf::SymtabEntry<C>,
        name: &[u8],
        output_section_id: OutputSectionId,
        value: u64,
        flags: ValueFlags,
    ) -> Result<&mut elf::SymtabEntry<C>> {
        let shndx = self
            .output_sections
            .output_index_of_section(output_section_id)
            .with_context(|| {
                format!(
                    "internal error: tried to copy symbol `{}` that's in section {} \
                     which is not being output",
                    String::from_utf8_lossy(name),
                    output_section_id,
                )
            })?;
        self.copy_symbol_shndx(sym, name, shndx, value, flags)
    }

    #[inline(always)]
    pub(crate) fn copy_symbol_shndx(
        &mut self,
        sym: &elf::SymtabEntry<C>,
        name: &[u8],
        shndx: u32,
        value: u64,
        flags: ValueFlags,
    ) -> Result<&mut elf::SymtabEntry<C>> {
        let e = LittleEndian;
        let is_local = flags.is_symtab_local(sym);
        let size = sym.st_size(e).into();
        let entry = self.define_symbol(
            is_local,
            SymbolSection::Index(shndx),
            value,
            size,
            Some(name),
        )?;
        entry.set_info(sym.st_info());
        entry.set_other(sym.st_other());
        // Fix binding if symbol was downgraded to local by version script
        if flags.is_downgraded_to_local() {
            entry.set_binding_and_type(object::elf::STB_LOCAL, sym.st_type());
        }
        Ok(entry)
    }

    pub(crate) fn copy_absolute_symbol(
        &mut self,
        sym: &elf::SymtabEntry<C>,
        name: &[u8],
        flags: ValueFlags,
    ) -> Result<&mut elf::SymtabEntry<C>> {
        let e = LittleEndian;
        let is_local = flags.is_symtab_local(sym);
        let value = sym.st_value(e).into();
        let size = sym.st_size(e).into();
        let entry = self.define_symbol(
            is_local,
            object::elf::SHN_ABS.into(),
            value,
            size,
            Some(name),
        )?;
        entry.set_info(sym.st_info());
        entry.set_other(sym.st_other());
        // Fix binding if symbol was downgraded to local by version script
        if flags.is_downgraded_to_local() {
            entry.set_binding_and_type(object::elf::STB_LOCAL, sym.st_type());
        }
        Ok(entry)
    }

    #[inline(always)]
    pub(crate) fn undefined_symbol(
        &mut self,
        is_local: bool,
        name: &[u8],
    ) -> Result<&mut elf::SymtabEntry<C>> {
        self.define_symbol(is_local, object::elf::SHN_UNDEF.into(), 0, 0, Some(name))
    }

    #[inline(always)]
    pub(crate) fn define_symbol(
        &mut self,
        is_local: bool,
        section: SymbolSection,
        value: u64,
        size: u64,
        name: Option<&[u8]>,
    ) -> Result<&mut elf::SymtabEntry<C>> {
        let (entry, symtab_shndx_entries) = if is_local {
            (
                self.local_entries.split_off_first_mut().with_context(|| {
                    format!(
                        "Insufficient .symtab local entries allocated for symbol `{}`",
                        String::from_utf8_lossy(name.unwrap_or_default()),
                    )
                })?,
                self.symtab_shndx_local_entries
                    .as_mut()
                    .and_then(|x| x.split_off_first_mut()),
            )
        } else {
            if self.is_dynamic {
                tracing::trace!(name = %String::from_utf8_lossy(name.unwrap_or_default()), "Write .dynsym");
            }
            (
                self.global_entries.split_off_first_mut().with_context(|| {
                    format!(
                        "Insufficient {} entries allocated for symbol `{}`",
                        if self.is_dynamic {
                            DYNSYM_SECTION_NAME_STR
                        } else {
                            ".symtab global"
                        },
                        String::from_utf8_lossy(name.unwrap_or_default()),
                    )
                })?,
                self.symtab_shndx_global_entries
                    .as_mut()
                    .and_then(|x| x.split_off_first_mut()),
            )
        };
        let string_offset = if let Some(name) = name {
            let name = if self.is_dynamic {
                // .dynsym encodes version info separately in .gnu.version, so strip it from the
                // name.
                crate::elf::RawSymbolName::parse(name).name
            } else {
                crate::elf::symtab_name_for_strtab(name)
            };
            self.strtab_writer.write_str(name)
        } else {
            0
        };

        let (index, shndx) = match section {
            SymbolSection::Raw(shndx) => (0, shndx),
            SymbolSection::Index(index) => {
                let shndx = object::elf::SymbolSection::new(index);
                if shndx == object::elf::SHN_XINDEX {
                    (index, shndx)
                } else {
                    (0, shndx)
                }
            }
        };
        if let Some(s) = symtab_shndx_entries {
            *s = index;
        } else if shndx == object::elf::SHN_XINDEX {
            bail!(
                "Expected .symtab_shndx section when writing symbol {} with shndx set to SHN_XINDEX.",
                String::from_utf8_lossy(name.unwrap_or_default())
            );
        }
        entry.set_name(string_offset);
        entry.set_info(object::elf::SymbolInfo(0));
        entry.set_other(object::elf::SymbolOther(0));
        entry.set_section(shndx);
        entry.set_value(value)?;
        entry.set_size(size)?;
        Ok(entry)
    }

    /// Verifies that we've used up all the space allocated to this writer. i.e. checks that we
    /// didn't allocate too much or missed writing something that we were supposed to write.
    pub(crate) fn check_exhausted(&self) -> Result {
        if !self.local_entries.is_empty()
            || !self.global_entries.is_empty()
            || !self.strtab_writer.out.is_empty()
        {
            let table_names = if self.is_dynamic {
                "dynsym/dynstr"
            } else {
                "symtab/strtab"
            };
            bail!(
                "Didn't use up all allocated {table_names} space. local={} global={} strings={}",
                self.local_entries.len(),
                self.global_entries.len(),
                self.strtab_writer.out.len()
            );
        }

        let symtab_shndx_local_len = self
            .symtab_shndx_local_entries
            .as_ref()
            .map_or(0, |s| s.len());
        let symtab_shndx_global_len = self
            .symtab_shndx_global_entries
            .as_ref()
            .map_or(0, |s| s.len());
        if symtab_shndx_local_len > 0 || symtab_shndx_global_len > 0 {
            bail!(
                "Didn't use up all allocated symtab_shndx space. local={} global={}",
                symtab_shndx_local_len,
                symtab_shndx_global_len,
            );
        }
        Ok(())
    }

    /// Returns a new writer that will take responsibility for the first `num_symbols`.
    pub(crate) fn take_prefix_global(&mut self, num_symbols: usize, strtab_size: usize) -> Self {
        Self {
            local_entries: &mut [],
            global_entries: self.global_entries.split_off_mut(..num_symbols).unwrap(),
            output_sections: self.output_sections,
            strtab_writer: self.strtab_writer.take_prefix(strtab_size),
            is_dynamic: self.is_dynamic,
            symtab_shndx_local_entries: None,
            symtab_shndx_global_entries: None,
        }
    }
}

pub(crate) fn build_sym_index_map<C: ElfClass>(layout: &ElfLayout<'_, C>) -> Vec<Option<u32>> {
    timing_phase!("Build sym index map");

    let section_sym_indices = build_section_sym_indices(layout);

    let num_all_locals = (layout
        .section_part_layouts
        .get(part_id::SYMTAB_LOCAL)
        .file_size
        / C::SYMTAB_ENTRY_SIZE as usize) as u32;

    let total_syms = layout.symbol_db.num_symbols();
    let mut map: Vec<Option<u32>> = vec![None; total_syms];

    // TODO: Use a ShardedWriter to parallelize this loop
    for group in &layout.group_layouts {
        let mut group_global_base = num_all_locals + group.symtab_global_start_index;
        let mut group_local_base = group.symtab_local_start_index;

        for file in &group.files {
            let FileLayout::Object(object) = file else {
                continue;
            };

            for ((sym_index, sym), flags) in object
                .object
                .enumerate_symbols()
                .zip(layout.per_symbol_flags.raw_range(object.symbol_id_range))
            {
                let symbol_id = object.symbol_id_range.input_to_id(sym_index);

                if sym.st_type() == object::elf::STT_SECTION
                    && let Ok(Some(input_section_index)) =
                        object.object.symbol_section(sym, sym_index)
                    && let Some(output_section_id) = match object.sections[input_section_index.0] {
                        SectionSlot::Loaded(_) | SectionSlot::MergeStrings(_) => Some(
                            object
                                .section_part_id(
                                    input_section_index,
                                    &layout.symbol_db.section_part_ids,
                                )
                                .output_section_id::<elf::Elf<C>>(),
                        ),
                        SectionSlot::FrameData(..) => Some(output_section_id::EH_FRAME),
                        _ => None,
                    }
                {
                    let primary_id = layout
                        .output_sections
                        .primary_output_section(output_section_id);
                    let sym_idx = section_sym_indices.get(primary_id);
                    map[symbol_id.as_usize()] = Some(*sym_idx);
                }

                if SymbolCopyInfo::new(
                    object.object,
                    sym_index,
                    sym,
                    symbol_id,
                    &layout.symbol_db,
                    flags.get(),
                    &object.sections,
                )
                .is_some()
                {
                    if flags.get().is_symtab_local(sym) {
                        map[symbol_id.as_usize()] = Some(group_local_base);
                        group_local_base += 1;
                    } else {
                        let canonical = layout.symbol_db.definition(symbol_id);
                        map[canonical.as_usize()] = Some(group_global_base);
                        group_global_base += 1;
                    }
                }
            }

            let e = LittleEndian;
            for (sym_index, sym) in object.object.symbols.enumerate() {
                if !sym.is_undefined(e) {
                    continue;
                }
                let symbol_id = object.symbol_id_range.input_to_id(sym_index);
                if !layout.symbol_db.is_canonical(symbol_id) {
                    continue;
                }
                if let Ok(name) = object.object.symbol_name(sym)
                    && !name.is_empty()
                {
                    map[symbol_id.as_usize()] = Some(group_global_base);
                    group_global_base += 1;
                }
            }
        }
    }

    map
}

pub(crate) fn build_section_sym_indices<C: ElfClass>(
    layout: &ElfLayout<'_, C>,
) -> OutputSectionMap<u32> {
    let mut map = OutputSectionMap::with_size(layout.output_sections.num_sections());
    let mut next_sym_idx: u32 = 1;
    for event in &layout.output_order {
        let OrderEvent::Section(section_id) = event else {
            continue;
        };
        if layout
            .output_sections
            .output_index_of_section(section_id)
            .is_none()
            || !layout
                .output_sections
                .will_emit_section_symbol_for_partial_objects(section_id)
        {
            continue;
        }
        *map.get_mut(section_id) = next_sym_idx;
        next_sym_idx += 1;
    }
    map
}

/// Writes debug symbols.
pub(crate) fn write_symbols<'data, C: ElfClass>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    for ((sym_index, sym), flags) in object
        .object
        .symbols
        .enumerate()
        .zip(layout.per_symbol_flags.raw_range(object.symbol_id_range))
    {
        let symbol_id = object.symbol_id_range.input_to_id(sym_index);

        if layout.symbol_db.args.got_plt_syms {
            write_got_plt_syms(layout, symbol_writer, symbol_id)?;
        }
        if let Some(info) = SymbolCopyInfo::new(
            object.object,
            sym_index,
            sym,
            symbol_id,
            &layout.symbol_db,
            flags.get(),
            &object.sections,
        ) {
            let Some(res) = layout.local_symbol_resolution(symbol_id) else {
                bail!("Missing resolution for {}", layout.symbol_debug(symbol_id));
            };

            let mut symbol_value = res.value_for_symbol_table();

            if sym.st_type() == object::elf::STT_TLS {
                symbol_value -= layout.tls_start_address();
            }

            symbol_writer
                .copy_object_symbol(
                    sym,
                    sym_index,
                    symbol_id,
                    info.name,
                    object,
                    layout,
                    symbol_value,
                    flags.get(),
                )
                .with_context(|| format!("Failed to copy {}", layout.symbol_debug(symbol_id)))?;
        }
    }

    if layout.args().should_output_partial_object() {
        for (sym_index, sym) in object.object.symbols.enumerate() {
            if !platform::Symbol::is_undefined(sym) {
                continue;
            }
            let Ok(name) = object.object.symbol_name(sym) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let symbol_id = object.symbol_id_range.input_to_id(sym_index);
            if !layout.symbol_db.is_canonical(symbol_id) {
                continue;
            }
            let name = RawSymbolName::parse(name).name;
            let entry = symbol_writer
                .undefined_symbol(false, name)
                .with_context(|| {
                    format!(
                        "Failed to write undefined symbol `{}` for partial link",
                        String::from_utf8_lossy(name)
                    )
                })?;
            entry.set_info(sym.st_info());
            entry.set_other(sym.st_other());
        }
    }

    Ok(())
}

pub(crate) fn write_got_plt_syms<C: ElfClass>(
    layout: &ElfLayout<C>,
    symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
    symbol_id: SymbolId,
) -> Result {
    if !layout.symbol_db.is_canonical(symbol_id) {
        return Ok(());
    }

    let Some(resolution) = layout.local_symbol_resolution(symbol_id) else {
        return Ok(());
    };

    if !resolution.flags.needs_got() {
        return Ok(());
    }

    let current_res_flags = resolution.flags;

    let mut write_sym =
        |suffix: &[u8],
         section_id: OutputSectionId,
         get_value: fn(&Resolution<elf::Elf<C>>) -> Result<u64>|
         -> Result {
            let mut symbol_name = layout.symbol_db.symbol_name(symbol_id)?.to_string();
            symbol_name.push_str(std::str::from_utf8(suffix).unwrap_or("unknown"));

            let shndx = layout
            .output_sections
            .output_index_of_section(section_id)
            .with_context(||format!(
                "Tried to write dynamic symbol in {section_id} section that's not being output"
            ))?;

            let value = get_value(resolution)?;

            symbol_writer
                .define_symbol(
                    true,
                    SymbolSection::Index(shndx),
                    value,
                    0,
                    Some(symbol_name.as_bytes()),
                )
                .with_context(|| {
                    format!(
                        "Failed to copy {} symbol for {}",
                        std::str::from_utf8(suffix).unwrap_or("unknown"),
                        layout.symbol_debug(symbol_id)
                    )
                })?;

            Ok(())
        };

    write_sym(b"$got", output_section_id::GOT, Resolution::got_address)?;
    if current_res_flags.needs_plt() {
        write_sym(b"$plt", output_section_id::PLT_GOT, Resolution::plt_address)?;
    }

    Ok(())
}

pub(crate) fn write_symbol_table_entries<C: ElfClass>(
    prelude: &PreludeLayout<elf::Elf<C>>,
    symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    // Define symbol 0. This needs to be a null placeholder.
    symbol_writer.undefined_symbol(true, &[])?;

    if layout.args().should_copy_input_relocs() {
        write_section_symbols(symbol_writer, layout)?;
    }

    let internal_symbols = &prelude.internal_symbols;

    write_internal_symbols(internal_symbols, layout, symbol_writer)?;
    Ok(())
}

pub(crate) fn write_section_symbols<C: ElfClass>(
    symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    for event in &layout.output_order {
        let OrderEvent::Section(section_id) = event else {
            continue;
        };
        let Some(shndx) = layout.output_sections.output_index_of_section(section_id) else {
            continue;
        };
        if !layout
            .output_sections
            .will_emit_section_symbol_for_partial_objects(section_id)
        {
            continue;
        }
        // Unnamed, matching GNU ld. Value is 0 for -r and the section VMA when fully linked.
        let value = if layout.args().should_output_partial_object() {
            0
        } else {
            layout.section_layouts.get(section_id).mem_offset
        };
        let entry =
            symbol_writer.define_symbol(true, SymbolSection::Index(shndx), value, 0, None)?;
        entry.set_binding_and_type(object::elf::STB_LOCAL, object::elf::STT_SECTION);
    }
    Ok(())
}

pub(crate) fn write_verdef<C: ElfClass>(
    verdefs: &[VersionDef],
    table_writer: &mut TableWriter<'_, '_, C>,
    soname: Option<&[u8]>,
    epilogue_offsets: &EpilogueOffsets,
) -> Result {
    let e = LittleEndian;

    // Offsets of version strings, except the base version
    let mut version_string_offsets = Vec::with_capacity(verdefs.len() - 1);

    for (i, verdef) in verdefs.iter().enumerate() {
        let verdef_out = table_writer.version_writer.take_verdef()?;

        // Base version may use (already allocated) soname
        let (name, name_offset) = if i == 0 {
            if let Some(soname) = soname {
                (
                    soname,
                    epilogue_offsets
                        .soname
                        .expect("Soname offset must be present at this point"),
                )
            } else {
                let offset = table_writer
                    .dynsym_writer
                    .strtab_writer
                    .write_str(&verdef.name);
                (verdef.name.as_slice(), offset)
            }
        } else {
            let offset = table_writer
                .dynsym_writer
                .strtab_writer
                .write_str(&verdef.name);
            version_string_offsets.push(offset);
            (verdef.name.as_slice(), offset)
        };

        verdef_out.vd_version.set(e, object::elf::VER_DEF_CURRENT);
        // Mark first entry as base version
        verdef_out.vd_flags.set(
            e,
            if i == 0 {
                object::elf::VER_FLG_BASE
            } else {
                object::elf::VersionFlags(0)
            },
        );
        verdef_out
            .vd_ndx
            .set(e, object::elf::VER_NDX_GLOBAL + i as u16);
        let aux_count = if verdef.parent_index.is_some() { 2 } else { 1 };
        verdef_out.vd_cnt.set(e, aux_count);
        verdef_out.vd_hash.set(e, object::elf::hash(name));
        verdef_out
            .vd_aux
            .set(e, size_of::<crate::elf::Verdef>() as u32);
        // Offset to the next entry, unless it's the last one
        let offset = if i < verdefs.len() - 1 {
            (size_of::<crate::elf::Verdef>()
                + size_of::<crate::elf::Verdaux>() * aux_count as usize) as u32
        } else {
            0
        };
        verdef_out.vd_next.set(e, offset);

        let verdaux = table_writer.version_writer.take_verdaux()?;
        verdaux.vda_name.set(e, name_offset);
        let next_vda = if verdef.parent_index.is_some() {
            size_of::<crate::elf::Verdaux>() as u32
        } else {
            0
        };
        verdaux.vda_next.set(e, next_vda);

        if let Some(parent_index) = &verdef.parent_index {
            let name_offset = *version_string_offsets
                .get(*parent_index as usize - 1)
                .unwrap();
            let verdaux = table_writer.version_writer.take_verdaux()?;
            verdaux.vda_name.set(e, name_offset);
            verdaux.vda_next.set(e, 0);
        }
    }

    Ok(())
}

pub(crate) fn write_dynamic_symbol_definitions<C: ElfClass>(
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    let chunk_size =
        10.max(layout.dynamic_symbol_definitions.len() / 10 / rayon::current_num_threads());

    layout
        .dynamic_symbol_definitions
        .chunks(chunk_size)
        .map(|defs| (defs, table_writer.take_dynsym_prefix(defs)))
        .par_bridge()
        .try_for_each(|(defs, mut table_writer)| {
            for sym_def in defs {
                let file_id = layout.symbol_db.file_id_for_symbol(sym_def.symbol_id);
                let file_layout = &layout.file_layout(file_id);
                match file_layout {
                    FileLayout::Object(object) => {
                        write_regular_object_dynamic_symbol_definition(
                            sym_def,
                            object,
                            layout,
                            &mut table_writer.dynsym_writer,
                        )?;

                        if let Some(versym) = table_writer.versym.as_mut() {
                            write_symbol_version(versym, sym_def.format_specific.version)?;
                        }
                    }
                    FileLayout::Dynamic(object) => {
                        write_copy_relocation_dynamic_symbol_definition(
                            sym_def,
                            object,
                            layout,
                            &mut table_writer.dynsym_writer,
                        )?;

                        if let Some(versym) = table_writer.versym.as_mut() {
                            copy_symbol_version(
                                object.object.symbol_versions(),
                                object.symbol_id_range.id_to_offset(sym_def.symbol_id),
                                &object.format_specific.version_mapping,
                                versym,
                            )?;
                        }
                    }
                    FileLayout::LinkerScript(script) => {
                        write_linker_script_dynsym(
                            &mut table_writer.dynsym_writer,
                            layout,
                            sym_def.symbol_id,
                            script,
                        )
                        .with_context(|| {
                            format!(
                                "Failed to write linker script dynsym: {}",
                                layout.symbol_debug(sym_def.symbol_id)
                            )
                        })?;
                    }
                    FileLayout::Prelude(prelude) => {
                        write_prelude_dynsym(
                            &mut table_writer.dynsym_writer,
                            layout,
                            sym_def.symbol_id,
                            prelude,
                        )?;
                        if let Some(versym) = table_writer.versym.as_mut() {
                            write_symbol_version(versym, sym_def.format_specific.version)?;
                        }
                    }
                    _ => bail!(
                        "Internal error: Unexpected dynamic symbol definition from {:?}. {}",
                        file_layout,
                        layout.symbol_debug(sym_def.symbol_id)
                    ),
                }
            }

            Ok(())
        })
}

/// Writes a symbol that was produced by a linker script.
pub(crate) fn write_linker_script_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    script: &LinkerScriptLayoutState<elf::Elf<C>>,
) -> Result {
    let local_index = script
        .internal_symbols
        .symbol_id_range()
        .id_to_offset(symbol_id);
    let info = &script.internal_symbols.symbol_definitions[local_index];
    write_internal_dynsym(dynsym_writer, layout, symbol_id, info)
}

/// Get the section index and type for a symbol.
/// This is used to copy attributes from a target symbol to a defsym alias.
pub(crate) fn get_symbol_attributes<C: ElfClass>(
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
) -> Result<(SymbolSection, object::elf::SymbolType)> {
    let file_id = layout.symbol_db.file_id_for_symbol(symbol_id);

    match layout.file_layout(file_id) {
        FileLayout::Object(obj) => {
            let local_index = symbol_id.to_input(obj.symbol_id_range);
            let sym = obj.object.symbol(local_index)?;

            let shndx = obj
                .object
                .symbol_section(sym, local_index)?
                .and_then(|section_index| {
                    let slot = &obj.sections[section_index.0];
                    match slot {
                        SectionSlot::Loaded(_)
                        | SectionSlot::MergeStrings(_)
                        | SectionSlot::Sorted(_) => {
                            let output_section_id = obj
                                .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                                .output_section_id::<elf::Elf<C>>();
                            layout
                                .output_sections
                                .output_index_of_section(output_section_id)
                        }
                        _ => None,
                    }
                })
                .map_or(object::elf::SHN_ABS.into(), SymbolSection::Index);

            let st_type = sym.st_type();

            Ok((shndx, st_type))
        }
        FileLayout::LinkerScript(script) => {
            let local_index = symbol_id.to_input(script.symbol_id_range);
            let def_info = &script.internal_symbols.symbol_definitions[local_index.0];
            let addr = layout
                .local_symbol_resolution(symbol_id)
                .map_or(0, |res| res.value());
            get_defsym_attributes(layout, def_info, addr)
        }
        FileLayout::Prelude(prelude) => {
            let offset = symbol_id.offset_from(SymbolId::undefined());
            let def_info = &prelude.internal_symbols.symbol_definitions[offset];
            let addr = layout
                .local_symbol_resolution(symbol_id)
                .map_or(0, |res| res.value());
            prelude_symbol_section_and_type(layout, def_info, addr)
        }
        FileLayout::SyntheticSymbols(_) => {
            // For other non-object files (e.g. epilogue), default to ABS
            Ok((object::elf::SHN_ABS.into(), object::elf::STT_NOTYPE))
        }
        FileLayout::Dynamic(_) | FileLayout::Epilogue(_) | FileLayout::NotLoaded => {
            Ok((object::elf::SHN_ABS.into(), object::elf::STT_NOTYPE))
        }
        FileLayout::StubLibrary(_) => unreachable!(),
    }
}

pub(crate) fn get_defsym_attributes<C: ElfClass>(
    layout: &ElfLayout<C>,
    def_info: &crate::parsing::InternalSymDefInfo<elf::Elf<C>>,
    addr: u64,
) -> Result<(SymbolSection, object::elf::SymbolType), error::Error> {
    let crate::parsing::SymbolPlacement::Redirect(redirect) = &def_info.placement else {
        unreachable!()
    };
    if let Expression::Symbol(target_name) = redirect.expression {
        let target_symbol_id =
            layout
                .symbol_db
                .get_unversioned(&crate::symbol::UnversionedSymbolName::prehashed(
                    target_name,
                ));

        if let Some(target_id) = target_symbol_id {
            let canonical_id = layout.symbol_db.definition(target_id);
            get_symbol_attributes(layout, canonical_id)
        } else if def_info.is_provide {
            Ok((object::elf::SHN_ABS.into(), object::elf::STT_NOTYPE))
        } else {
            Err(redirect.missing_target(target_name))
        }
    } else {
        let shndx = match redirect.loc {
            SymbolLoc::SectionEnd(os) => {
                let os = layout.output_sections.primary_output_section(os);
                layout.output_sections.output_index_of_nearest_section(os)
            }
            SymbolLoc::SectionStartRelative(os) | SymbolLoc::SectionEndRelative(os) => {
                let os = layout.output_sections.primary_output_section(os);
                layout
                    .output_sections
                    .output_index_of_section(os)
                    .or_else(|| output_index_of_nearby_section(layout, os, addr))
            }
            SymbolLoc::FirstSection => Some(1),
            SymbolLoc::LocationCounter(_, Some(os)) => {
                let os = layout.output_sections.primary_output_section(os);
                layout
                    .output_sections
                    .output_index_of_section(os)
                    .or_else(|| layout.output_sections.output_index_of_nearest_section(os))
            }
            SymbolLoc::LocationCounter(_, None) => Some(1),
            SymbolLoc::None => return Ok((object::elf::SHN_ABS.into(), object::elf::STT_NOTYPE)),
        };
        Ok((
            shndx.map_or(
                SymbolSection::Raw(object::elf::SHN_ABS),
                SymbolSection::Index,
            ),
            object::elf::STT_NOTYPE,
        ))
    }
}

pub(crate) fn section_is_loaded<A: crate::platform::SectionAttributes>(attr: &A) -> bool {
    attr.is_alloc() && !attr.is_no_bits()
}

/// GNU ld `SEC_READONLY`. Unflagged empty sections are not readonly; `!SHF_WRITE`
/// only counts once the section is allocated.
pub(crate) fn section_is_readonly<A: crate::platform::SectionAttributes>(attr: &A) -> bool {
    attr.is_alloc() && !attr.is_writable()
}

/// GNU ld `_bfd_nearby_section`: map a symbol whose output section was omitted
/// onto a neighbouring kept section in the same segment.
pub(crate) fn output_index_of_nearby_section<C: ElfClass>(
    layout: &ElfLayout<C>,
    section_id: OutputSectionId,
    addr: u64,
) -> Option<u32> {
    let os = &layout.output_sections;
    let prev_id = os.previous_emitted_section_id(section_id);
    let next_id = os.following_emitted_section_id(section_id);
    let best_id = match (prev_id, next_id) {
        (None, None) => return None,
        (Some(prev), None) => prev,
        (None, Some(next)) => next,
        (Some(prev), Some(next)) => {
            // GNU ld often never sets SEC_ALLOC/SEC_LOAD on an empty omitted
            // section (no input contributed flags). Wild may already have
            // PHDR-derived ALLOC, which would pick the wrong neighbour.
            let omitted = <elf::Elf<C> as Platform>::SectionAttributes::default();
            let s = &omitted;
            let prev_attr = &os.output_info(prev).section_attributes;
            let next_attr = &os.output_info(next).section_attributes;
            let alloc_tls_load_differ = prev_attr.is_alloc() != next_attr.is_alloc()
                || prev_attr.is_tls() != next_attr.is_tls()
                || section_is_loaded(prev_attr) != section_is_loaded(next_attr);
            if alloc_tls_load_differ {
                if next_attr.is_alloc() != s.is_alloc()
                    || next_attr.is_tls() != s.is_tls()
                    || (section_is_loaded(prev_attr) && !section_is_loaded(next_attr))
                {
                    prev
                } else {
                    next
                }
            } else if section_is_readonly(prev_attr) != section_is_readonly(next_attr) {
                if section_is_readonly(next_attr) != section_is_readonly(s) {
                    prev
                } else {
                    next
                }
            } else if prev_attr.is_executable() != next_attr.is_executable() {
                if next_attr.is_executable() != s.is_executable() {
                    prev
                } else {
                    next
                }
            } else {
                let next_vma = layout.merged_section_layouts.get(next).mem_offset;
                if addr < next_vma { prev } else { next }
            }
        }
    };
    os.output_index_of_section(best_id)
}

pub(crate) fn write_prelude_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    prelude: &PreludeLayout<elf::Elf<C>>,
) -> Result {
    let offset = symbol_id.offset_from(prelude.internal_symbols.start_symbol_id);
    let def_info = prelude
        .internal_symbols
        .symbol_definitions
        .get(offset)
        .with_context(|| format!("Invalid prelude symbol {}", layout.symbol_debug(symbol_id)))?;
    write_internal_dynsym(dynsym_writer, layout, symbol_id, def_info)
}

pub(crate) fn write_internal_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    def_info: &crate::parsing::InternalSymDefInfo<elf::Elf<C>>,
) -> Result {
    if matches!(
        def_info.placement,
        crate::parsing::SymbolPlacement::Redirect(_)
    ) {
        return write_defsym_dynsym(dynsym_writer, layout, symbol_id, def_info);
    }

    let section_id = def_info
        .section_id()
        .context("Tried to export dynamic symbol not associated with a section")?;

    let section_id = layout.output_sections.primary_output_section(section_id);

    let shndx = layout
        .output_sections
        .output_index_of_section(section_id)
        .context("Tried to write dynamic symbol in section that's not being output")?;

    let resolution = layout
        .local_symbol_resolution(symbol_id)
        .with_context(|| format!("Missing resolution for {}", layout.symbol_debug(symbol_id)))?;

    let address = resolution.address()?;
    let name = layout.symbol_db.symbol_name(symbol_id)?;

    let entry = dynsym_writer.define_symbol(
        false,
        SymbolSection::Index(shndx),
        address,
        0,
        Some(name.bytes()),
    )?;
    entry.set_binding_and_type(object::elf::STB_GLOBAL, object::elf::STT_NOTYPE);

    Ok(())
}

/// Writes a dynsym entry for a symbol defined via --defsym or linker script symbol assignment.
pub(crate) fn write_defsym_dynsym<C: ElfClass>(
    dynsym_writer: &mut SymbolTableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    symbol_id: SymbolId,
    def_info: &crate::parsing::InternalSymDefInfo<elf::Elf<C>>,
) -> Result {
    let resolution = layout
        .local_symbol_resolution(symbol_id)
        .with_context(|| format!("Missing resolution for {}", layout.symbol_debug(symbol_id)))?;
    let address = resolution.raw_value;
    let (shndx, st_type) = get_defsym_attributes(layout, def_info, address)?;
    let name = layout.symbol_db.symbol_name(symbol_id)?;

    let entry = dynsym_writer
        .define_symbol(false, shndx, address, 0, Some(name.bytes()))
        .with_context(|| {
            format!(
                "Failed to define dynamic {}",
                layout.symbol_debug(symbol_id)
            )
        })?;
    entry.set_binding_and_type(object::elf::STB_GLOBAL, st_type);

    Ok(())
}

pub(crate) fn write_copy_relocation_dynamic_symbol_definition<'data, C: ElfClass>(
    sym_def: &crate::layout::DynamicSymbolDefinition<elf::Elf<C>>,
    object: &DynamicLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
    dynamic_symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    debug_assert_bail!(
        layout
            .flags_for_symbol(sym_def.symbol_id)
            .needs_copy_relocation(),
        "Tried to write copy relocation for symbol without COPY_RELOCATION flag"
    );
    let sym_index = sym_def.symbol_id.to_input(object.symbol_id_range);
    let sym = object.object.symbol(sym_index)?;
    let name = sym_def.name;
    let shndx = layout
        .output_sections
        .output_index_of_section(output_section_id::BSS)
        .context("Copy relocation with no BSS section")?;
    let res = layout
        .local_symbol_resolution(sym_def.symbol_id)
        .context("Copy relocation for unresolved symbol")?;
    dynamic_symbol_writer
        .copy_symbol_shndx(sym, name, shndx, res.raw_value, ValueFlags::empty())
        .with_context(|| {
            format!(
                "Failed to copy dynamic {}",
                layout.symbol_debug(sym_def.symbol_id)
            )
        })?;
    Ok(())
}

pub(crate) fn write_regular_object_dynamic_symbol_definition<'data, C: ElfClass>(
    sym_def: &crate::layout::DynamicSymbolDefinition<elf::Elf<C>>,
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
    dynamic_symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    let sym_index = sym_def.symbol_id.to_input(object.symbol_id_range);
    let sym = object.object.symbol(sym_index)?;
    let name = sym_def.name;
    let section_index = object.object.symbol_section(sym, sym_index)?;
    if section_index.is_none()
        && !platform::Symbol::is_common(sym)
        && !platform::Symbol::is_absolute(sym)
    {
        dynamic_symbol_writer
            .copy_symbol_shndx(sym, name, 0, 0, ValueFlags::empty())
            .with_context(|| {
                format!(
                    "Failed to copy dynamic {}",
                    layout.symbol_debug(sym_def.symbol_id)
                )
            })?;
        return Ok(());
    }

    let symbol_id = sym_def.symbol_id;
    let resolution = layout.local_symbol_resolution(symbol_id).with_context(|| {
        format!(
            "Tried to write dynamic symbol definition without a resolution: {}",
            layout.symbol_debug(symbol_id)
        )
    })?;

    // For non-PIE executables, export IFUNC symbols as STT_FUNC pointing to PLT stub.
    // For PIE executables, keep IFUNC as-is.
    if section_index.is_some()
        && resolution.flags.is_ifunc()
        && layout.symbol_db.output_kind.is_executable()
        && !layout.symbol_db.output_kind.is_position_independent()
        && let Some(plt_address) = resolution.format_specific.plt_address
    {
        let plt_output_section_id = layout
            .output_sections
            .primary_output_section(output_section_id::PLT_GOT);
        let shndx = dynamic_symbol_writer
            .output_sections
            .output_index_of_section(plt_output_section_id)
            .with_context(|| {
                format!(
                    "PLT section not found for ifunc symbol `{}`",
                    String::from_utf8_lossy(name),
                )
            })?;
        let size = object_symbol_size(sym, sym_index, object)?;
        let entry = dynamic_symbol_writer.define_symbol(
            false,
            SymbolSection::Index(shndx),
            plt_address.into(),
            size,
            Some(name),
        )?;
        entry.set_binding_and_type(sym.st_bind(), object::elf::STT_FUNC);
        entry.set_other(sym.st_other());
    } else {
        let mut symbol_value = resolution.value_for_symbol_table();
        if sym.st_type() == object::elf::STT_TLS {
            symbol_value -= layout.tls_start_address();
        }

        dynamic_symbol_writer
            .copy_object_symbol(
                sym,
                sym_index,
                symbol_id,
                name,
                object,
                layout,
                symbol_value,
                ValueFlags::empty(),
            )
            .with_context(|| {
                format!("Failed to copy dynamic {}", layout.symbol_debug(symbol_id))
            })?;
    }
    Ok(())
}

/// Section index and type for a prelude (or script-overridden prelude) symbol.
///
/// When a linker script assigns the same name (`_etext = .`), GNU ld attaches the
/// symbol to that script section, not to the unused builtin `.text`.
pub(crate) fn prelude_symbol_section_and_type<C: ElfClass>(
    layout: &ElfLayout<C>,
    def_info: &crate::parsing::InternalSymDefInfo<elf::Elf<C>>,
    addr: u64,
) -> Result<(SymbolSection, object::elf::SymbolType)> {
    if matches!(
        def_info.placement,
        crate::parsing::SymbolPlacement::Redirect(_)
    ) {
        return get_defsym_attributes(layout, def_info, addr);
    }
    if let Some(script_def) = crate::layout::script_assignment_def(def_info.name, &layout.symbol_db)
        && matches!(
            script_def.placement,
            crate::parsing::SymbolPlacement::Redirect(_)
        )
    {
        return get_defsym_attributes(layout, script_def, addr);
    }

    let shndx = def_info
        .section_id()
        .and_then(|section_id| {
            let section_id = layout.output_sections.primary_output_section(section_id);
            layout.output_sections.output_index_of_section(section_id)
        })
        .map_or(object::elf::SHN_ABS.into(), SymbolSection::Index);

    Ok((shndx, def_info.symbol.st_type()))
}

pub(crate) fn write_internal_symbols<C: ElfClass>(
    internal_symbols: &InternalSymbols<elf::Elf<C>>,
    layout: &ElfLayout<C>,
    symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    for (local_index, def_info) in internal_symbols.symbol_definitions.iter().enumerate() {
        if def_info.name.is_empty() {
            continue;
        }
        if def_info.is_provide
            && let crate::parsing::SymbolPlacement::Redirect(redirect) = &def_info.placement
        {
            let mut missing_rhs = false;
            redirect.expression.visit_expressions(&mut |e| {
                if let Expression::Symbol(name) = e
                    && layout
                        .symbol_db
                        .get_unversioned(&crate::symbol::UnversionedSymbolName::prehashed(name))
                        .is_none()
                {
                    missing_rhs = true;
                }
                true
            });
            if missing_rhs {
                continue;
            }
        }
        let symbol_id = internal_symbols.start_symbol_id.add_usize(local_index);
        if !layout.symbol_db.is_canonical(symbol_id) {
            continue;
        }
        let Some(resolution) = layout.local_symbol_resolution(symbol_id) else {
            continue;
        };

        let symbol_name = layout.symbol_db.symbol_name(symbol_id)?;

        let mut address = resolution.value();

        // For Redirect, get attributes from the target symbol. A linker-script assignment
        // of the same name (e.g. `_etext = .`) overrides the prelude section, so `_etext` is
        // in the script `.text` rather than SHN_ABS from the unused builtin.
        let (mut shndx, st_type) = prelude_symbol_section_and_type(layout, def_info, address)?;

        // Move symbols that are in our header (section 0) into the first section, otherwise they'll
        // show up as undefined.
        if matches!(shndx, SymbolSection::Index(0)) {
            shndx = SymbolSection::Index(1);
        }

        if platform::Symbol::is_tls(&def_info.symbol) {
            address -= layout.tls_start_address();
        }

        // Mandatory RISC-V symbol defined by the default linker script as:
        // __global_pointer$ = MIN(__SDATA_BEGIN__ + 0x800, MAX(__DATA_BEGIN__ + 0x800, __BSS_END__
        // - 0x800));
        if symbol_name.bytes() == GLOBAL_POINTER_SYMBOL_NAME.as_bytes() {
            address += RISCV_TLS_DTV_OFFSET;
        }

        // PROVIDE_HIDDEN symbols should be local, not global
        let st_bind = if platform::Symbol::is_hidden(&def_info.symbol) {
            object::elf::STB_LOCAL
        } else {
            object::elf::STB_GLOBAL
        };

        let entry = symbol_writer
            .define_symbol(
                st_bind == object::elf::STB_LOCAL,
                shndx,
                address,
                0,
                Some(symbol_name.bytes()),
            )
            .with_context(|| format!("Failed to write {}", layout.symbol_debug(symbol_id)))?;

        entry.set_binding_and_type(st_bind, st_type);
    }
    Ok(())
}

pub(crate) fn copy_symbol_version(
    versym_in: &[Versym],
    local_symbol_index: usize,
    version_mapping: &[object::elf::VersionIndex],
    versym_out: &mut &mut [Versym],
) -> Result {
    let output_version =
        versym_in
            .get(local_symbol_index)
            .map_or(object::elf::VER_NDX_GLOBAL, |versym| {
                let input_version = versym.0.get(LittleEndian).index();
                if input_version.is_special() {
                    input_version
                } else {
                    version_mapping[usize::from(input_version - object::elf::VER_NDX_GLOBAL)]
                }
            });

    write_symbol_version(versym_out, output_version.into())
}

pub(crate) fn write_symbol_version(
    versym_out: &mut &mut [Versym],
    version: object::elf::VersymIndex,
) -> Result {
    versym_out
        .split_off_first_mut()
        .context("Insufficient .gnu.version allocation")?
        .0
        .set(LittleEndian, version);

    Ok(())
}

pub(crate) struct StrTabWriter<'out> {
    pub(crate) next_offset: u32,
    pub(crate) out: &'out mut [u8],
}

impl StrTabWriter<'_> {
    /// Writes a string to the string table. Returns the offset within the string table at which the
    /// string was written.
    pub(crate) fn write_str(&mut self, str: &[u8]) -> u32 {
        let len_with_terminator = str.len() + 1;
        let lib_name_out = self.out.split_off_mut(..len_with_terminator).unwrap();
        lib_name_out[..str.len()].copy_from_slice(str);
        lib_name_out[str.len()] = 0;
        let offset = self.next_offset;
        self.next_offset += len_with_terminator as u32;
        offset
    }

    pub(crate) fn take_prefix(&mut self, size: usize) -> Self {
        let next_offset = self.next_offset;
        self.next_offset += size as u32;

        Self {
            next_offset,
            out: self.out.split_off_mut(..size).unwrap(),
        }
    }
}
