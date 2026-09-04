use crate::alignment::Alignment;
#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::gnu::*;
use crate::elf::output_section_id;
use crate::elf::part_id;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout;
use crate::layout::CommonGroupState;
use crate::layout::EnginePlatform;
use crate::layout::HandlerData as _;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform::ObjectFile;
use crate::platform::Symbol as _;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use hashbrown::HashMap;
use itertools::Itertools as _;
use rayon::prelude::*;

/// Where we've decided that we need copy relocations, look for symbols with the same address as the
/// symbols with copy relocations. If the other symbol is non-weak, then we do the copy relocation
/// for that symbol instead. We also request dynamic symbol definitions for each copy relocation.
/// For that reason, this needs to be done before we merge dynamic symbol definitions.
pub(crate) fn finalise_copy_relocations<'data, C: ElfClass>(
    group_states: &mut [layout::GroupState<'data, Elf<C>>],
    symbol_db: &SymbolDb<'data, Elf<C>>,
    symbol_flags: &AtomicPerSymbolFlags,
) -> Result {
    timing_phase!("Finalise copy relocations");

    group_states.par_iter_mut().try_for_each(|group| {
        verbose_timing_phase!("Finalise copy relocations for group");
        for file in &mut group.files {
            if let layout::FileLayoutState::Dynamic(dynamic) = file {
                // Skip iterating over our symbol table if we don't have any copy relocations.
                if dynamic.format_specific.copy_relocations.is_empty() {
                    continue;
                }

                select_copy_relocation_alternatives(
                    dynamic,
                    symbol_flags,
                    &mut group.common,
                    symbol_db,
                )?;
            }
        }

        Ok(())
    })
}

/// Looks for any non-weak symbols at the same addresses as any of our copy relocations. If
/// found, we'll generate the copy relocation for the strong symbol instead of weak symbol at
/// the same address.
pub(crate) fn select_copy_relocation_alternatives<'data, C: ElfClass>(
    state: &mut layout::DynamicLayoutState<'data, Elf<C>>,
    per_symbol_flags: &AtomicPerSymbolFlags,
    common: &mut CommonGroupState<'data, Elf<C>>,
    symbol_db: &SymbolDb<'data, Elf<C>>,
) -> Result {
    for (i, symbol) in state.object.enumerate_symbols() {
        let address = symbol.value();
        let Some(info) = state.format_specific.copy_relocations.get_mut(&address) else {
            continue;
        };

        let symbol_id = state.symbol_id_range.input_to_id(i);

        if !symbol_db.is_canonical(symbol_id) {
            continue;
        }

        layout::export_dynamic(common, symbol_id, symbol_db)?;

        per_symbol_flags
            .get_atomic(symbol_id)
            .fetch_or(ValueFlags::COPY_RELOCATION);

        if symbol.is_weak() || !info.is_weak || info.symbol_id == symbol_id {
            continue;
        }

        info.symbol_id = symbol_id;
        info.is_weak = false;
    }

    Ok(())
}

pub(crate) fn allocate_for_copy_relocations<'data, C: ElfClass>(
    state: &layout::DynamicLayoutState<'data, Elf<C>>,
    common: &mut CommonGroupState<'data, Elf<C>>,
) -> Result {
    for value in state.format_specific.copy_relocations.values() {
        let symbol_id = value.symbol_id;

        let symbol_index = state.symbol_id_range().id_to_input(symbol_id);
        let symbol = state.object.symbol(symbol_index)?;

        let section_index = state
            .object
            .symbol_section(symbol, symbol_index)?
            .context("Copy relocation for undefined symbol")?;
        let section = state.object.section(section_index)?;

        let alignment = Alignment::new(state.object.section_alignment(section)?)?;

        // Allocate space in BSS for the copy of the symbol.
        let size = symbol.size();
        common.allocate(
            output_section_id::BSS.part_id_with_alignment::<Elf<C>>(alignment),
            alignment.align_up(size),
        );

        // Allocate space required for the copy relocation itself.
        common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
    }

    Ok(())
}

pub(crate) fn assign_copy_relocation_addresses<'data, C: ElfClass>(
    state: &layout::DynamicLayoutState<'data, Elf<C>>,
    copy_relocation_symbols: &[SymbolId],
    memory_offsets: &mut OutputSectionPartMap<u64>,
) -> Result<HashMap<u64, u64>> {
    copy_relocation_symbols
        .iter()
        .map(|symbol_id| {
            let symbol_index = state.symbol_id_range.id_to_input(*symbol_id);
            let symbol = state.object.symbol(symbol_index)?;

            let section_index = state
                .object
                .symbol_section(symbol, symbol_index)?
                .context("Copy relocation for undefined symbol")?;
            let section = state.object.section(section_index)?;

            let alignment = Alignment::new(state.object.section_alignment(section)?)?;

            let input_address = symbol.value();
            let output_address =
                assign_copy_relocation_address::<C>(alignment, symbol.size(), memory_offsets);

            Ok((input_address, output_address))
        })
        .try_collect()
}

/// Assigns the address in BSS for the copy relocation of a symbol.
pub(crate) fn assign_copy_relocation_address<C: ElfClass>(
    alignment: Alignment,
    size: u64,
    memory_offsets: &mut OutputSectionPartMap<u64>,
) -> u64 {
    let bss =
        memory_offsets.get_mut(output_section_id::BSS.part_id_with_alignment::<Elf<C>>(alignment));
    let a = *bss;
    *bss += alignment.align_up(size);
    a
}

impl CopyRelocationInfo {
    pub(crate) fn add_symbol<'data, P: EnginePlatform>(
        &mut self,
        symbol_id: SymbolId,
        is_weak: bool,
        resources: &layout::GraphResources<'data, '_, P>,
    ) {
        if self.symbol_id == symbol_id || is_weak {
            return;
        }

        if !self.is_weak {
            resources.symbol_db.warning(format!(
                "Multiple non-weak symbols at the same address have copy relocations: {}, {}",
                resources.symbol_debug(self.symbol_id),
                resources.symbol_debug(symbol_id)
            ));
        }

        self.symbol_id = symbol_id;
        self.is_weak = false;
    }
}
