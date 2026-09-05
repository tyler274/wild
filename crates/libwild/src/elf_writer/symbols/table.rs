use super::super::types::*;
use crate::bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::GLOBAL_POINTER_SYMBOL_NAME;
use crate::elf::RawSymbolName;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::FileLayout;
use crate::layout::InternalSymbols;
use crate::layout::ObjectLayout;
use crate::layout::PreludeLayout;
use crate::layout::Resolution;
use crate::layout::SymbolCopyInfo;
use crate::linker_script::Expression;
use crate::linker_script::RelocatableAnchor;
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
use object::SectionIndex;
use object::SymbolIndex;
use object::elf::STT_TLS;
use object::read::elf::Sym as _;

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
    strtab_lookup: Option<&'layout elf::FinalizedStrtab>,
}

impl<'layout, 'out, C: ElfClass> SymbolTableWriter<'layout, 'out, C> {
    pub(crate) fn new(
        start_string_offset: u32,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
        output_sections: &'layout OutputSections<'layout, elf::Elf<C>>,
        strtab_lookup: Option<&'layout elf::FinalizedStrtab>,
    ) -> Result<Self> {
        let local_entries = slice_from_all_bytes_mut(buffers.take(part_id::SYMTAB_LOCAL));
        let global_entries = slice_from_all_bytes_mut(buffers.take(part_id::SYMTAB_GLOBAL));
        let symtab_shndx_local_entries = Some(buffers.take(part_id::SYMTAB_SHNDX_LOCAL))
            .and_then(|s| (!s.is_empty()).then(|| slice_from_all_bytes_mut(s)));
        let symtab_shndx_global_entries = Some(buffers.take(part_id::SYMTAB_SHNDX_GLOBAL))
            .and_then(|s| (!s.is_empty()).then(|| slice_from_all_bytes_mut(s)));

        let strings = buffers.take(part_id::STRTAB);
        let mut strtab_writer = StrTabWriter {
            next_offset: start_string_offset,
            out: strings,
        };
        if let Some(table) = strtab_lookup {
            strtab_writer.write_finalized(table)?;
        }
        Ok(Self {
            local_entries,
            global_entries,
            output_sections,
            strtab_writer,
            is_dynamic: false,
            symtab_shndx_local_entries,
            symtab_shndx_global_entries,
            strtab_lookup,
        })
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
            strtab_lookup: None,
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

        let entry = if let Some(section_index) = object.object.symbol_section(sym, sym_index)? {
            self.copy_symbol_with_section(
                sym,
                symbol_id,
                name,
                object,
                layout,
                value,
                flags,
                section_index,
            )?
        } else if sym.is_common(e) {
            let section_id = if sym.st_type() == STT_TLS {
                output_section_id::TBSS
            } else {
                output_section_id::BSS
            };

            Some(self.copy_symbol(sym, name, section_id, value, flags)?)
        } else if sym.is_absolute(e) {
            self.copy_absolute_symbol(sym, name, flags)
                .with_context(|| {
                    format!("Failed to absolute {}", layout.symbol_debug(symbol_id))
                })?;
            return Ok(());
        } else {
            bail!("Attempted to output a symtab entry with an unexpected section type")
        };

        if let Some(entry) = entry {
            entry.set_size(object_symbol_size(sym, sym_index, object)?)?;
        }

        Ok(())
    }

    fn copy_symbol_with_section(
        &mut self,
        sym: &elf::SymtabEntry<C>,
        symbol_id: SymbolId,
        name: &[u8],
        object: &ObjectLayout<elf::Elf<C>>,
        layout: &ElfLayout<C>,
        value: u64,
        flags: ValueFlags,
        section_index: SectionIndex,
    ) -> Result<Option<&mut elf::SymtabEntry<C>>> {
        let section_id = match &object.sections[section_index.0] {
            SectionSlot::Loaded(_)
            | SectionSlot::Sorted(_)
            | SectionSlot::LoadedDebugInfo(_)
            | SectionSlot::MergeStrings(_) => object
                .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                .output_section_id::<elf::Elf<C>>(),
            SectionSlot::FrameData(..) => output_section_id::EH_FRAME,
            _ => {
                if layout.symbol_db.is_mapping_symbol(symbol_id) {
                    return Ok(None);
                }
                bail!(
                    "Tried to copy a symbol in a section we didn't load. {}",
                    layout.symbol_debug(symbol_id)
                )
            }
        };
        let section_id = layout.output_sections.primary_output_section(section_id);
        Ok(Some(self.copy_symbol(sym, name, section_id, value, flags)?))
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
            if let Some(lookup) = self.strtab_lookup {
                lookup.offset(name)?
            } else {
                self.strtab_writer.write_str(name)
            }
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
            strtab_lookup: self.strtab_lookup,
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
                        | SectionSlot::Sorted(_)
                        | SectionSlot::Unloaded(_)
                        | SectionSlot::MustLoad(_)
                        | SectionSlot::LoadedDebugInfo(_) => {
                            let output_section_id = obj
                                .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                                .output_section_id::<elf::Elf<C>>();
                            // Later matchers in a script output section are
                            // secondaries and have no output index. Map to the
                            // primary like `copy_object_symbol` (kernel
                            // `jiffies = jiffies_64` before `SECTIONS`).
                            let output_section_id = layout
                                .output_sections
                                .primary_output_section(output_section_id);
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
    match redirect.expression.relocatable_anchor() {
        Some(RelocatableAnchor::Symbol(target_name)) => {
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
        }
        Some(RelocatableAnchor::LocationCounter) => {
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
                SymbolLoc::None => {
                    return Ok((object::elf::SHN_ABS.into(), object::elf::STT_NOTYPE));
                }
            };
            Ok((
                shndx.map_or(
                    SymbolSection::Raw(object::elf::SHN_ABS),
                    SymbolSection::Index,
                ),
                object::elf::STT_NOTYPE,
            ))
        }
        None => Ok((object::elf::SHN_ABS.into(), object::elf::STT_NOTYPE)),
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

    fn write_finalized(&mut self, table: &elf::FinalizedStrtab) -> Result {
        if self.out.is_empty() {
            return Ok(());
        }
        if self.out.len() != table.bytes.len() {
            bail!(
                "Allocated {} bytes for .strtab, but suffix-merged table is {} bytes",
                self.out.len(),
                table.bytes.len()
            );
        }
        self.out.copy_from_slice(&table.bytes);
        self.out = &mut [];
        Ok(())
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
