use super::dynamic::*;
use super::symbols::*;
use crate::OutputKind;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::debug_assert_bail;
use crate::elf;
use crate::elf::EhFrameHdr;
use crate::elf::EhFrameHdrEntry;
use crate::elf::ElfClass;
use crate::elf::ElfWord as _;
use crate::elf::part_id;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::excessive_allocation;
use crate::file_writer::insufficient_allocation;
use crate::layout::Layout;
use crate::layout::Resolution;
use crate::layout::compute_allocations;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform::Arch;
use crate::writable_elf::WritableRela as _;
use crate::writable_elf::WritableRelr as _;
use linker_utils::elf::DynamicRelocationKind;
use linker_utils::utils::slice_from_all_bytes_mut;
use std::ops::Not as _;
use std::ops::Range;
use std::ops::Sub;
use zerocopy::FromBytes;

pub(crate) type ElfLayout<'data, C> = Layout<'data, elf::Elf<C>>;

pub(crate) struct TableWriter<'layout, 'out, C: ElfClass> {
    pub(crate) output_kind: OutputKind,
    pub(crate) got: &'out mut [elf::Word<C>],
    pub(crate) got_relr: &'out mut [elf::Word<C>],
    pub(crate) plt_got: &'out mut [u8],
    pub(crate) rela_plt: &'out mut [elf::Rela<C>],
    pub(crate) tls: Range<u64>,
    pub(crate) rela_dyn_relative: &'out mut [elf::Rela<C>],
    pub(crate) rela_dyn_general: &'out mut [elf::Rela<C>],
    pub(crate) relr_dyn: Option<&'out mut [elf::Relr<C>]>,
    pub(crate) current_relr_dyn: Option<&'out mut elf::Relr<C>>,
    /// RELR run state for bitmap packing.
    pub(crate) relr_writer: elf::RelrEncoder<C>,
    pub(crate) dynsym_writer: SymbolTableWriter<'layout, 'out, C>,
    pub(crate) debug_symbol_writer: SymbolTableWriter<'layout, 'out, C>,
    pub(crate) eh_frame_start_address: u64,
    pub(crate) eh_frame: &'out mut [u8],

    /// Note, this is stored as raw bytes because it starts with an EhFrameHdr, but is then
    /// followed by multiple EhFrameHdrEntry.
    pub(crate) eh_frame_hdr: &'out mut [u8],

    pub(crate) dynamic: DynamicEntriesWriter<'out, C>,
    pub(crate) version_writer: VersionWriter<'out>,
}

impl<'layout, 'out, C: ElfClass> TableWriter<'layout, 'out, C> {
    pub(crate) fn from_layout(
        layout: &'layout ElfLayout<C>,
        dynstr_start_offset: u32,
        strtab_start_offset: u32,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
        eh_frame_start_address: u64,
    ) -> TableWriter<'layout, 'out, C> {
        let dynsym_writer = SymbolTableWriter::<C>::new_dynamic(
            dynstr_start_offset,
            buffers,
            &layout.output_sections,
        );
        let debug_symbol_writer =
            SymbolTableWriter::<C>::new(strtab_start_offset, buffers, &layout.output_sections);

        Self::new(
            layout.symbol_db.output_kind,
            layout.tls_start_address()..layout.tls_end_address(),
            buffers,
            dynsym_writer,
            debug_symbol_writer,
            eh_frame_start_address,
            layout.symbol_db.args.is_relr_enabled(),
        )
    }

    pub(crate) fn new(
        output_kind: OutputKind,
        tls: Range<u64>,
        buffers: &mut OutputSectionPartMap<&'out mut [u8]>,
        dynsym_writer: SymbolTableWriter<'layout, 'out, C>,
        debug_symbol_writer: SymbolTableWriter<'layout, 'out, C>,
        eh_frame_start_address: u64,
        pack_relative_relocs: bool,
    ) -> TableWriter<'layout, 'out, C> {
        let eh_frame = buffers.take(part_id::EH_FRAME);
        let eh_frame_hdr = buffers.take(part_id::EH_FRAME_HDR);
        let dynamic = DynamicEntriesWriter::new(buffers.take(part_id::DYNAMIC));
        let versym = slice_from_all_bytes_mut(buffers.take(part_id::GNU_VERSION));
        let version_writer = VersionWriter::new(
            buffers.take(part_id::GNU_VERSION_D),
            buffers.take(part_id::GNU_VERSION_R),
            versym.is_empty().not().then_some(versym),
        );

        TableWriter {
            output_kind,
            got: <[elf::Word<C>]>::mut_from_bytes(buffers.take(part_id::GOT)).unwrap(),
            got_relr: <[elf::Word<C>]>::mut_from_bytes(buffers.take(part_id::GOT_RELR)).unwrap(),
            plt_got: buffers.take(part_id::PLT_GOT),
            rela_plt: slice_from_all_bytes_mut(buffers.take(part_id::RELA_PLT)),
            tls,
            rela_dyn_relative: slice_from_all_bytes_mut(buffers.take(part_id::RELA_DYN_RELATIVE)),
            rela_dyn_general: slice_from_all_bytes_mut(buffers.take(part_id::RELA_DYN_GENERAL)),
            relr_dyn: pack_relative_relocs
                .then(|| slice_from_all_bytes_mut(buffers.take(part_id::RELR_DYN)))
                .filter(|b| !b.is_empty()),
            current_relr_dyn: None,
            relr_writer: elf::RelrEncoder::<C>::default(),
            dynsym_writer,
            debug_symbol_writer,
            eh_frame_start_address,
            eh_frame,
            eh_frame_hdr,
            dynamic,
            version_writer,
        }
    }

    pub(crate) fn process_resolution<'data, A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        layout: Option<&ElfLayout<'data, C>>,
        args: &ElfArgs,
        res: &Resolution<elf::Elf<C>>,
    ) -> Result {
        let Some(got_address) = res.format_specific.got_address else {
            return Ok(());
        };

        let mut got_address = got_address.get();
        let flags = res.flags;

        // For TLS variables, we'll generally only have one of these, but we might have all 3
        // combinations.
        if flags.needs_got_tls_offset()
            || flags.needs_got_tls_module()
            || flags.needs_got_tls_descriptor()
        {
            if flags.needs_got_tls_offset() {
                self.process_got_tls_offset::<A>(
                    res,
                    layout.context("Layout must be present")?,
                    got_address,
                )?;
                got_address += C::GOT_ENTRY_SIZE;
            }
            if flags.needs_got_tls_module() {
                self.process_got_tls_mod_and_offset::<A>(res, args, got_address)?;
                got_address += 2 * C::GOT_ENTRY_SIZE;
            }
            if flags.needs_got_tls_descriptor() {
                self.process_got_tls_descriptor::<A>(res, args, got_address)?;
            }
            return Ok(());
        }

        let has_dynamic_symbol =
            res.flags.is_dynamic() || (flags.needs_export_dynamic() && res.flags.is_interposable());
        let is_got_relr =
            crate::elf::is_got_relr_eligible(res.flags, has_dynamic_symbol, args, self.output_kind);
        let got_entry = if is_got_relr {
            self.take_next_got_relr_entry()?
        } else {
            self.take_next_got_entry()?
        };
        if res.flags.is_dynamic()
            || (flags.needs_export_dynamic() && res.flags.is_interposable())
                && !res.flags.is_ifunc()
        {
            *got_entry = elf::Word::<C>::from_u64(0)?;
            debug_assert_bail!(
                compute_allocations::<elf::Elf<C>>(res, self.output_kind, args)
                    .get(part_id::RELA_DYN_GENERAL)
                    > 0,
                "Tried to write glob-dat with no allocation. {}",
                res.flags
            );
            self.write_dynamic_symbol_relocation::<A>(
                got_address,
                0,
                res.dynamic_symbol_index()?,
                DynamicRelocationKind::GotEntry,
            )?;
        } else if res.flags.is_ifunc() {
            *got_entry = elf::Word::<C>::from_u64(0)?;
            self.write_ifunc_relocation::<A>(res)?;
        } else {
            let value = if is_got_relr {
                // GOT_RELR entries are bitmap-packed by write_got_relr_bitmap - just store value.
                res.raw_value
            } else if res.flags.has_link_time_address()
                && self.output_kind.is_position_independent()
            {
                self.write_relr_entry_flat::<A>(got_address, res.raw_value)?
            } else {
                res.raw_value
            };
            *got_entry = elf::Word::<C>::from_u64(value)?;
        }
        if let Some(plt_address) = res.format_specific.plt_address {
            self.write_plt_entry::<A>(got_address, plt_address.get())?;
        }

        // For ifunc symbols with GOT-relative references, write the PLT stub
        // address to the separate GOT entry. This ensures that all references to the IFUNC
        // return the same address (the PLT stub), regardless of whether they go through the
        // PLT or directly through GOT.
        if res.flags.needs_ifunc_got_for_address() {
            let ifunc_got_address = got_address + C::GOT_ENTRY_SIZE;
            let got_entry = self.take_next_got_entry()?;
            let plt_address = res.plt_address()?;
            let value = if self.output_kind.is_position_independent() {
                self.write_relr_entry_flat::<A>(ifunc_got_address, plt_address)?
            } else {
                plt_address
            };
            *got_entry = elf::Word::<C>::from_u64(value)?;
        }

        Ok(())
    }

    pub(crate) fn process_got_tls_offset<'data, A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        res: &Resolution<elf::Elf<C>>,
        layout: &ElfLayout<'data, C>,
        got_address: u64,
    ) -> Result {
        let got_entry = self.take_next_got_entry()?;
        if res.flags.is_dynamic()
            || (res.flags.needs_export_dynamic() && res.flags.is_interposable())
        {
            *got_entry = elf::Word::<C>::from_u64(0)?;
            return self.write_tpoff_relocation::<A>(got_address, res.dynamic_symbol_index()?, 0);
        }
        let address = res.raw_value;
        if address == 0 {
            // Resolution is undefined.
            *got_entry = elf::Word::<C>::from_u64(0)?;
            return Ok(());
        }
        // TLS_MODULE_BASE points at the end of the .tbss in some cases, thus relax the
        // verification.
        if !(self.tls.start..=self.tls.end).contains(&address) {
            bail!(
                "GotTlsOffset resolves to address not in TLS segment 0x{:x}",
                address
            );
        }
        if self.output_kind.is_executable() {
            // Convert the address to an offset relative to the TCB.

            *got_entry =
                elf::Word::<C>::from_u64(address.wrapping_sub(A::tp_offset_start(layout)))?;
        } else {
            debug_assert_bail!(
                compute_allocations::<elf::Elf<C>>(res, self.output_kind, layout.args())
                    .get(part_id::RELA_DYN_GENERAL)
                    > 0,
                "Tried to write tpoff with no allocation. {}",
                res.flags
            );
            self.write_tpoff_relocation::<A>(got_address, 0, address.sub(self.tls.start) as i64)?;
        }
        Ok(())
    }

    pub(crate) fn process_got_tls_mod_and_offset<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        res: &Resolution<elf::Elf<C>>,
        args: &ElfArgs,
        got_address: u64,
    ) -> Result {
        let got_entry = self.take_next_got_entry()?;
        if self.output_kind.is_executable() && !res.flags.is_dynamic() {
            *got_entry = elf::Word::<C>::from_u64(elf::CURRENT_EXE_TLS_MOD)?;
        } else {
            *got_entry = elf::Word::<C>::from_u64(0)?;
            let dynamic_symbol_index = res.dynamic_symbol_index.map_or(0, std::num::NonZero::get);
            debug_assert_bail!(
                compute_allocations::<elf::Elf<C>>(res, self.output_kind, args)
                    .get(part_id::RELA_DYN_GENERAL)
                    > 0,
                "Tried to write dtpmod with no allocation. {}",
                res.flags
            );
            self.write_dtpmod_relocation::<A>(got_address, dynamic_symbol_index)?;
        }
        let offset_entry = self.take_next_got_entry()?;
        if let Some(dynamic_symbol_index) = res.dynamic_symbol_index {
            if res.flags.is_interposable() {
                self.write_dtpoff_relocation::<A>(
                    got_address + C::GOT_ENTRY_SIZE,
                    dynamic_symbol_index.get(),
                )?;
            }
            *offset_entry = elf::Word::<C>::from_u64(0)?;
            return Ok(());
        }
        // Convert the address to an offset within the TLS segment
        let address = res.address()?;
        *offset_entry = elf::Word::<C>::from_u64(
            address
                .wrapping_sub(self.tls.start)
                .wrapping_sub(A::get_dtv_offset()),
        )?;
        Ok(())
    }

    pub(crate) fn process_got_tls_descriptor<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        res: &Resolution<elf::Elf<C>>,
        args: &ElfArgs,
        got_address: u64,
    ) -> Result {
        // TLS descriptor occupies 2 entries
        *self.take_next_got_entry()? = elf::Word::<C>::from_u64(0)?;
        *self.take_next_got_entry()? = elf::Word::<C>::from_u64(0)?;

        ensure!(
            !self.output_kind.is_static_executable(),
            "Cannot create dynamic TLSDESC relocation (function trampoline will be missed) for a static executable"
        );

        let dynamic_symbol_index = res.dynamic_symbol_index.map_or(0, std::num::NonZero::get);
        debug_assert_bail!(
            compute_allocations::<elf::Elf<C>>(res, self.output_kind, args)
                .get(part_id::RELA_DYN_GENERAL)
                > 0,
            "Tried to write TLS descriptor with no allocation. {}",
            res.flags
        );
        let addend = if res.dynamic_symbol_index.is_none() {
            res.raw_value.sub(self.tls.start) as i64
        } else {
            0
        };
        self.write_tls_descriptor_relocation::<A>(got_address, dynamic_symbol_index, addend)?;

        Ok(())
    }

    pub(crate) fn write_plt_entry<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        got_address: u64,
        plt_address: u64,
    ) -> Result {
        let plt_entry = self.take_plt_got_entry()?;
        A::write_plt_entry(plt_entry, got_address, plt_address)
    }

    pub(crate) fn take_plt_got_entry(&mut self) -> Result<&'out mut [u8]> {
        if self.plt_got.len() < elf::PLT_ENTRY_SIZE as usize {
            bail!("Didn't allocate enough space in .plt.got");
        }
        Ok(self
            .plt_got
            .split_off_mut(..elf::PLT_ENTRY_SIZE as usize)
            .unwrap())
    }

    pub(crate) fn take_next_got_entry(&mut self) -> Result<&'out mut elf::Word<C>> {
        self.got
            .split_off_first_mut()
            .ok_or_else(|| insufficient_allocation(".got"))
    }

    pub(crate) fn take_next_got_relr_entry(&mut self) -> Result<&'out mut elf::Word<C>> {
        self.got_relr
            .split_off_first_mut()
            .ok_or_else(|| insufficient_allocation(".got (relr)"))
    }

    /// Resets RELR run state between input sections.
    /// Layout tracks runs per-section; writer must do the same to stay in sync.
    pub(crate) fn reset_relr_run(&mut self) {
        self.relr_writer = elf::RelrEncoder::<C>::default();
        self.current_relr_dyn = None;
    }

    /// Writes bitmap-packed RELR entries for the entire GOT_RELR block.
    /// Called after all symbol resolutions are processed.
    pub(crate) fn write_got_relr_bitmap(&mut self, n: u64, base: u64) -> Result {
        if base == 0 || n == 0 {
            return Ok(());
        }
        let Some(relr_writer) = &mut self.relr_dyn else {
            return Ok(());
        };
        // Write address entry for base.
        let entry = relr_writer
            .split_off_first_mut()
            .ok_or_else(|| insufficient_allocation(".relr.dyn"))?;
        entry.set_value(base)?;
        // Write bitmap entries for remaining n-1 slots.
        let mut remaining = n - 1;
        while remaining > 0 {
            let slots = remaining.min(elf::relr_bitmap_slots::<C>());
            let bitmap: u64 = ((1u64 << slots) - 1) << 1 | 1;
            let entry = relr_writer
                .split_off_first_mut()
                .ok_or_else(|| insufficient_allocation(".relr.dyn"))?;
            entry.set_value(bitmap)?;
            remaining = remaining.saturating_sub(elf::relr_bitmap_slots::<C>());
        }
        Ok(())
    }

    /// Checks that we used all of the entries that we requested during layout.
    pub(crate) fn validate_empty(&self, mem_sizes: &OutputSectionPartMap<u64>) -> Result {
        if !self.got.is_empty() {
            return Err(excessive_allocation(
                ".got",
                self.got.len() as u64 * C::GOT_ENTRY_SIZE,
                mem_sizes.get(part_id::GOT),
            ));
        }
        if !self.got_relr.is_empty() {
            return Err(excessive_allocation(
                ".got (relr)",
                self.got_relr.len() as u64 * C::GOT_ENTRY_SIZE,
                mem_sizes.get(part_id::GOT_RELR),
            ));
        }
        if !self.rela_dyn_relative.is_empty() {
            return Err(excessive_allocation(
                ".rela.dyn (relative)",
                self.rela_dyn_relative.len() as u64 * C::RELA_ENTRY_SIZE,
                mem_sizes.get(part_id::RELA_DYN_RELATIVE),
            ));
        }
        if !self.rela_dyn_general.is_empty() {
            return Err(excessive_allocation(
                ".rela.dyn (general)",
                self.rela_dyn_general.len() as u64 * C::RELA_ENTRY_SIZE,
                mem_sizes.get(part_id::RELA_DYN_GENERAL),
            ));
        }
        if let Some(relr_dyn) = &self.relr_dyn
            && !relr_dyn.is_empty()
        {
            return Err(excessive_allocation(
                ".relr.dyn",
                relr_dyn.len() as u64 * C::RELR_ENTRY_SIZE,
                mem_sizes.get(part_id::RELR_DYN),
            ));
        }
        self.dynsym_writer.check_exhausted()?;
        self.debug_symbol_writer.check_exhausted()?;
        self.version_writer.check_exhausted(mem_sizes)?;
        if !self.eh_frame.is_empty() {
            return Err(excessive_allocation(
                ".eh_frame",
                self.eh_frame.len() as u64,
                mem_sizes.get(part_id::EH_FRAME),
            ));
        }
        if !self.eh_frame_hdr.is_empty() {
            return Err(excessive_allocation(
                ".eh_frame_hdr",
                self.eh_frame_hdr.len() as u64,
                mem_sizes.get(part_id::EH_FRAME_HDR),
            ));
        }
        if !self.dynamic.out.is_empty() {
            return Err(excessive_allocation(
                ".dynamic",
                std::mem::size_of_val(self.dynamic.out) as u64,
                mem_sizes.get(part_id::DYNAMIC),
            ));
        }
        Ok(())
    }

    pub(crate) fn write_ifunc_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        res: &Resolution<elf::Elf<C>>,
    ) -> Result {
        let out = self.rela_plt.split_off_first_mut().unwrap();
        out.set_addend(res.raw_value as i64)?;
        let got_address = res
            .format_specific
            .got_address
            .context("Missing GOT entry for ifunc")?
            .get();
        out.set_offset(got_address)?;
        out.set_info(
            0,
            A::get_dynamic_relocation_type(DynamicRelocationKind::Irelative),
        )?;
        Ok(())
    }

    pub(crate) fn write_dtpmod_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        dynamic_symbol_index: u32,
    ) -> Result {
        self.write_rela_dyn_general(
            place,
            dynamic_symbol_index,
            A::get_dynamic_relocation_type(DynamicRelocationKind::DtpMod),
            0,
        )
    }

    pub(crate) fn write_tls_descriptor_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        dynamic_symbol_index: u32,
        addend: i64,
    ) -> Result {
        self.write_rela_dyn_general(
            place,
            dynamic_symbol_index,
            A::get_dynamic_relocation_type(DynamicRelocationKind::TlsDesc),
            addend,
        )
    }

    pub(crate) fn write_dtpoff_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        dynamic_symbol_index: u32,
    ) -> Result {
        self.write_rela_dyn_general(
            place,
            dynamic_symbol_index,
            A::get_dynamic_relocation_type(DynamicRelocationKind::DtpOff),
            0,
        )
    }

    pub(crate) fn write_tpoff_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        dynamic_symbol_index: u32,
        addend: i64,
    ) -> Result {
        self.write_rela_dyn_general(
            place,
            dynamic_symbol_index,
            A::get_dynamic_relocation_type(DynamicRelocationKind::TpOff),
            addend,
        )
    }

    /// Writes a single flat RELR address entry without bitmap packing.
    /// Used for GOT-based RELR entries where layout counts flat (one entry per slot).
    /// Falls back to rela.dyn.relative when RELR is not enabled.
    // TODO: Implement bitmap packing for GOT-based RELR entries. Requires splitting
    // the GOT into two parts so relative relocations are contiguous and countable
    // during layout.
    pub(crate) fn write_relr_entry_flat<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        relative_address: u64,
    ) -> Result<u64> {
        if let Some(relr_writer) = &mut self.relr_dyn
            && place.is_multiple_of(2)
        {
            let entry = relr_writer
                .split_off_first_mut()
                .ok_or_else(|| insufficient_allocation(".relr.dyn"))?;
            entry.set_value(place)?;
            Ok(relative_address)
        } else {
            let rela = self
                .rela_dyn_relative
                .split_off_first_mut()
                .ok_or_else(|| insufficient_allocation(".rela.dyn (relative)"))?;
            rela.set_offset(place)?;
            rela.set_addend(relative_address as i64)?;
            rela.set_info(
                0,
                A::get_dynamic_relocation_type(DynamicRelocationKind::Relative),
            )?;
            Ok(0)
        }
    }

    #[inline(always)]
    /// Writes RELA or RELR entry and returns value that should be written at the relocation site.
    pub(crate) fn write_address_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        relative_address: u64,
    ) -> Result<u64> {
        debug_assert_bail!(
            self.output_kind.is_position_independent(),
            "write_address_relocation called when output is not position-independent"
        );
        // Odd offsets can't be encoded as RELR address entries (LSB used as bitmap
        // marker), so fall back to RELA for them.
        if let Some(relr_writer) = &mut self.relr_dyn
            && place.is_multiple_of(2)
        {
            self.relr_writer.encode(place, |encoded, encoding| {
                match encoding {
                    elf::RelrEntryEncoding::New => {
                        let entry = relr_writer
                            .split_off_first_mut()
                            .ok_or_else(|| insufficient_allocation(".relr.dyn"))?;
                        entry.set_value(encoded)?;
                        self.current_relr_dyn = Some(entry);
                    }
                    elf::RelrEntryEncoding::Update => {
                        let entry = self
                            .current_relr_dyn
                            .as_deref_mut()
                            .ok_or_else(|| error!("Internal error in RELR bitmap encoding"))?;
                        entry.set_value(encoded)?;
                    }
                }
                Ok(())
            })?;
            Ok(relative_address)
        } else {
            let rela = self
                .rela_dyn_relative
                .split_off_first_mut()
                .ok_or_else(|| insufficient_allocation(".rela.dyn (relative)"))?;
            rela.set_offset(place)?;
            rela.set_addend(relative_address as i64)?;
            rela.set_info(
                0,
                A::get_dynamic_relocation_type(DynamicRelocationKind::Relative),
            )?;
            Ok(0)
        }
    }

    pub(crate) fn write_ifunc_relocation_for_data<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        resolver_address: i64,
    ) -> Result {
        // IRELATIVE relocations go in .rela.dyn general section, not the relative section,
        // because the dynamic linker expects only R_X86_64_RELATIVE in the relative section.
        self.write_rela_dyn_general(
            place,
            0, // No dynamic symbol for IRELATIVE
            A::get_dynamic_relocation_type(DynamicRelocationKind::Irelative),
            resolver_address,
        )
    }

    pub(crate) fn write_dynamic_symbol_relocation<A: Arch<Platform = elf::Elf<C>>>(
        &mut self,
        place: u64,
        addend: i64,
        symbol_index: u32,
        kind: DynamicRelocationKind,
    ) -> Result {
        let _span = tracing::trace_span!("write_dynamic_symbol_relocation").entered();
        debug_assert_bail!(
            self.output_kind.needs_dynsym(),
            "Tried to write dynamic relocation without a dynamic symbol table"
        );
        let rela = self.take_rela_dyn()?;
        rela.set_offset(place)?;
        rela.set_addend(addend)?;
        rela.set_info(symbol_index, A::get_dynamic_relocation_type(kind))?;
        Ok(())
    }

    pub(crate) fn write_rela_dyn_general(
        &mut self,
        place: u64,
        dynamic_symbol_index: u32,
        r_type: object::elf::RelocationType,
        addend: i64,
    ) -> Result {
        debug_assert_bail!(
            self.output_kind.needs_dynsym(),
            "write_rela_dyn_general called when output is not dynamic"
        );
        let rela = self.take_rela_dyn()?;
        rela.set_offset(place)?;
        rela.set_addend(addend)?;
        rela.set_info(dynamic_symbol_index, r_type)?;
        Ok(())
    }

    pub(crate) fn take_rela_dyn(&mut self) -> Result<&mut elf::Rela<C>> {
        tracing::trace!("Consume .rela.dyn general");
        self.rela_dyn_general
            .split_off_first_mut()
            .ok_or_else(|| insufficient_allocation(".rela.dyn (non-relative)"))
    }

    pub(crate) fn take_eh_frame_hdr(&mut self) -> &'out mut EhFrameHdr {
        let entry_bytes = self
            .eh_frame_hdr
            .split_off_mut(..size_of::<EhFrameHdr>())
            .unwrap();
        EhFrameHdr::mut_from_bytes(entry_bytes).unwrap()
    }

    pub(crate) fn take_eh_frame_hdr_entry(&mut self) -> Option<&mut EhFrameHdrEntry> {
        if self.eh_frame_hdr.is_empty() {
            return None;
        }
        let entry_bytes = self
            .eh_frame_hdr
            .split_off_mut(..size_of::<EhFrameHdrEntry>())
            .unwrap();
        Some(EhFrameHdrEntry::mut_from_bytes(entry_bytes).unwrap())
    }

    pub(crate) fn take_eh_frame_data(&mut self, size: usize) -> Result<&'out mut [u8]> {
        if size > self.eh_frame.len() {
            return Err(insufficient_allocation(".eh_frame"));
        }
        Ok(self.eh_frame.split_off_mut(..size).unwrap())
    }

    pub(crate) fn write_eh_frame_terminator(&mut self) {
        // Ignore insufficient capacity so that we don't error if .eh_frame is empty.
        if let Ok(buf) = self.take_eh_frame_data(size_of::<u32>()) {
            buf.fill(0);
        }
    }

    /// Takes a prefix of dynsym, dynstr and versym suitable for writing the supplied definitions.
    pub(crate) fn take_dynsym_prefix(
        &mut self,
        defs: &[crate::layout::DynamicSymbolDefinition<elf::Elf<C>>],
    ) -> VersionedDynsymWriter<'layout, 'out, C> {
        let num_symbols = defs.len();
        let strtab_size = defs.iter().map(|d| d.name.len() + 1).sum();

        VersionedDynsymWriter {
            dynsym_writer: self
                .dynsym_writer
                .take_prefix_global(num_symbols, strtab_size),
            versym: self.version_writer.take_prefix(num_symbols),
        }
    }
}
