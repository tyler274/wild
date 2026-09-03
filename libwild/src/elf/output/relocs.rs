use super::*;
use crate::bail;
#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::gnu::*;
use crate::elf::part_id;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::error::Result;
use crate::layout;
use crate::layout::CommonGroupState;
use crate::layout::ObjectLayoutState;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Relaxation as _;
use crate::platform::Relocation;
use crate::platform::SectionFlags as _;
use crate::platform::SectionHeader as _;
use crate::symbol_db::SymbolId;
use crate::value_flags::ValueFlags;
use linker_utils::elf::RelocationKind;
use linker_utils::relaxation::RelocationModifier;
use object::LittleEndian;
use object::read::elf::SectionHeader as _;
use rayon::Scope;
use std::sync::atomic;

#[inline(always)]
pub(crate) fn process_relocation<
    'data,
    'scope,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    object: &ObjectLayoutState<'data, Elf<C>>,
    common: &mut CommonGroupState<'data, Elf<C>>,
    rel: &R,
    section: &<A::Platform as Platform>::SectionHeader,
    section_part_id: PartId,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    is_debug_section: bool,
    scope: &Scope<'scope>,
    relr_writer: &mut RelrEncoder<C>,
) -> Result<RelocationModifier> {
    let Some(local_sym_index) = rel.symbol() else {
        return Ok(RelocationModifier::Normal);
    };

    let mut classified =
        classify_symbol_relocation::<C, A, R>(object, rel, section, local_sym_index, resources)?;

    materialize_relocation_requirements::<C, A, R>(
        common,
        rel,
        section,
        resources,
        is_debug_section,
        relr_writer,
        &mut classified,
    )?;

    let previous_flags =
        note_relocation_symbol_reference::<C, A>(&classified, resources, queue, scope);

    if !is_debug_section {
        crate::thunks::handle_thunk_extensions_for_relocation::<A>(
            section_part_id,
            resources,
            classified.local_symbol_id,
            classified.symbol_id,
            classified.r_type,
        );
    }

    layout::check_for_undefined::<A>(
        object,
        section,
        classified.rel_offset,
        local_sym_index,
        classified.flags,
        classified.symbol_id,
        resources,
    )?;

    if classified.flags_to_add.needs_copy_relocation() && !previous_flags.needs_copy_relocation() {
        queue.send_copy_relocation_request::<A>(classified.symbol_id, resources, scope);
    }

    Ok(classified.next_modifier)
}

/// Symbol and relocation-kind info needed for both GC edges and output accounting.
pub(crate) struct ClassifiedSymbolRelocation {
    pub(crate) local_symbol_id: SymbolId,
    pub(crate) symbol_id: SymbolId,
    /// Definition (and local) value flags before this relocation's contributions.
    pub(crate) flags: ValueFlags,
    pub(crate) flags_to_add: ValueFlags,
    pub(crate) rel_offset: u64,
    pub(crate) r_type: object::elf::RelocationType,
    pub(crate) rel_kind: linker_utils::elf::RelocationKind,
    pub(crate) next_modifier: RelocationModifier,
    pub(crate) section_is_writable: bool,
}

/// Resolve the relocated symbol and determine the effective relocation kind / initial flags.
#[inline(always)]
pub(crate) fn classify_symbol_relocation<
    'data,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    object: &ObjectLayoutState<'data, Elf<C>>,
    rel: &R,
    section: &<A::Platform as Platform>::SectionHeader,
    local_sym_index: object::SymbolIndex,
    resources: &layout::GraphResources<'data, '_, Elf<C>>,
) -> Result<ClassifiedSymbolRelocation> {
    let args = resources.symbol_db.args;
    let symbol_db = resources.symbol_db;
    let local_symbol_id = object.symbol_id_range.input_to_id(local_sym_index);
    let symbol_id = symbol_db.definition(local_symbol_id);
    let mut flags = resources.local_flags_for_symbol(symbol_id);
    flags.merge(resources.local_flags_for_symbol(local_symbol_id));
    let rel_offset = rel.offset();
    let r_type = rel.raw_type();
    let section_flags = section.sh_flags(LittleEndian);

    let mut next_modifier = RelocationModifier::Normal;
    let rel_info = if let Some(relaxation) = A::new_relaxation(
        r_type,
        object.object.raw_section_data(section)?,
        rel_offset,
        flags,
        symbol_db.output_kind,
        section_flags,
        None,
        1,
        0,
        0,
        None,
    )
    .filter(|relaxation| args.should_relax() || relaxation.is_mandatory())
    {
        next_modifier = relaxation.next_modifier();
        relaxation.rel_info()
    } else {
        A::relocation_from_raw(r_type)?
    };

    Ok(ClassifiedSymbolRelocation {
        local_symbol_id,
        symbol_id,
        flags,
        flags_to_add: layout::resolution_flags(rel_info.kind),
        rel_offset,
        r_type,
        rel_kind: rel_info.kind,
        next_modifier,
        section_is_writable: section.is_writable(),
    })
}

/// Account for GOT/PLT/dynamic-reloc/TLS sizes implied by this relocation.
#[inline(always)]
pub(crate) fn materialize_relocation_requirements<
    'data,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    common: &mut CommonGroupState<'data, Elf<C>>,
    rel: &R,
    section: &<A::Platform as Platform>::SectionHeader,
    resources: &layout::GraphResources<'data, '_, Elf<C>>,
    is_debug_section: bool,
    relr_writer: &mut RelrEncoder<C>,
    classified: &mut ClassifiedSymbolRelocation,
) -> Result {
    let args = resources.symbol_db.args;
    let symbol_db = resources.symbol_db;
    let section_flags = section.sh_flags(LittleEndian);
    let flags = classified.flags;
    let symbol_id = classified.symbol_id;
    let r_type = classified.r_type;
    let section_is_writable = classified.section_is_writable;
    let flags_to_add = &mut classified.flags_to_add;
    let rel_kind = classified.rel_kind;

    if !section_flags.is_alloc() {
        // Non-alloc sections never get dynamic relocations, so there's nothing to do here.
    } else if rel_kind.is_tls() {
        if does_relocation_require_static_tls(rel_kind) {
            resources
                .has_static_tls
                .store(true, atomic::Ordering::Relaxed);
        }

        if layout::needs_tlsld(rel_kind)
            && !resources
                .layout_resources_ext
                .uses_tlsld
                .load(atomic::Ordering::Relaxed)
        {
            resources
                .layout_resources_ext
                .uses_tlsld
                .store(true, atomic::Ordering::Relaxed);
        }
    } else if flags_to_add.needs_direct() && flags.is_interposable() {
        if symbol_db.output_kind.is_shared_object()
            && A::is_disallowed_for_interposable_symbols(r_type)
        {
            bail!(
                "relocation {} cannot be used when making a shared object; \
                recompile with -fPIC",
                A::rel_type_to_string(r_type),
            );
        }
        if section_is_writable {
            common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
        } else if flags.is_function() {
            // Create a PLT entry for the function and refer to that instead.
            flags_to_add.remove(ValueFlags::DIRECT);
            *flags_to_add |= ValueFlags::PLT | ValueFlags::GOT | ValueFlags::CANONICAL_PLT;
        } else if !flags.is_absolute() {
            match args.copy_relocations_enabled() {
                crate::args::CopyRelocations::Allowed => {
                    *flags_to_add |= ValueFlags::COPY_RELOCATION;
                }
                crate::args::CopyRelocations::Disallowed(reason) => {
                    // We don't at present support text relocations, so if we can't apply a copy
                    // relocation, we error instead.
                    bail!(
                        "Direct relocation ({}) to dynamic symbol from non-writable section, \
                        but copy relocations are disabled because {reason}. {}",
                        A::rel_type_to_string(r_type),
                        resources.symbol_debug(symbol_id),
                    );
                }
            }
        }
    } else if flags.is_ifunc()
        && rel_kind == RelocationKind::Absolute
        && section_is_writable
        && symbol_db.output_kind.is_position_independent()
    {
        common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
    } else if symbol_db.output_kind.is_position_independent()
        && rel_kind == RelocationKind::Absolute
        && flags.has_link_time_address()
    {
        if section_is_writable {
            // Odd offsets can't be encoded as RELR address entries (LSB used as
            // bitmap marker), so fall back to RELA for them.
            if resources.symbol_db.args.is_relr_enabled() && rel.offset().is_multiple_of(2) {
                relr_writer.encode(rel.offset(), |_, encoding| {
                    if matches!(encoding, RelrEntryEncoding::New) {
                        common.allocate(part_id::RELR_DYN, C::RELR_ENTRY_SIZE);
                    }
                    Ok(())
                })?;
            } else {
                common.allocate(part_id::RELA_DYN_RELATIVE, C::RELA_ENTRY_SIZE);
            }
        } else if !is_debug_section {
            bail!(
                "Cannot apply relocation {} to read-only section. \
                Please recompile with -fPIC or link with -no-pie",
                A::rel_type_to_string(r_type),
            );
        }
    }

    // For ifunc symbols with GOT-relative references (like R_X86_64_GOTPCRELX), we need a
    // separate GOT entry for address equality. The main GOT entry will be used by the PLT stub
    // with an IRELATIVE relocation, while this extra entry will contain the PLT stub address so
    // that all references to the ifunc return the same address.

    let relocation_needs_got = flags_to_add.needs_got();
    let relocation_needs_got_for_address = relocation_needs_got && !flags_to_add.needs_plt();

    if flags.is_function() && relocation_needs_got_for_address {
        *flags_to_add |= ValueFlags::GOT_FOR_PLT_ENTRY;
    }

    if flags.is_ifunc() && !symbol_db.output_kind.is_static_executable() {
        *flags_to_add |= ValueFlags::GOT | ValueFlags::PLT;
    }

    if flags.is_ifunc() && relocation_needs_got && symbol_db.output_kind.has_fixed_load_address() {
        *flags_to_add |= ValueFlags::IFUNC_GOT_FOR_ADDRESS;
    }

    Ok(())
}

/// Record that a live section references `classified.symbol_id` and enqueue graph work if needed.
#[inline(always)]
pub(crate) fn note_relocation_symbol_reference<
    'data,
    'scope,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
>(
    classified: &ClassifiedSymbolRelocation,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    scope: &Scope<'scope>,
) -> ValueFlags {
    let symbol_id = classified.symbol_id;
    let flags = classified.flags;
    let flags_to_add = classified.flags_to_add;

    let atomic_flags = &resources.per_symbol_flags.get_atomic(symbol_id);
    let previous_flags = atomic_flags.fetch_or(flags_to_add);

    if !previous_flags.has_resolution() {
        if flags.is_ifunc() && resources.symbol_db.output_kind.is_static_executable() {
            atomic_flags.fetch_or(ValueFlags::GOT | ValueFlags::PLT);
        }

        queue.send_symbol_request::<A>(symbol_id, resources, scope);
    }

    previous_flags
}

/// Returns whether the supplied relocation type requires static TLS. If true and we're writing a
/// shared object, then the STATIC_TLS will be set in the shared object which is a signal to the
/// runtime loader that the shared object cannot be loaded at runtime (e.g. with dlopen).
pub(crate) fn does_relocation_require_static_tls(rel_kind: RelocationKind) -> bool {
    layout::resolution_flags(rel_kind) == ValueFlags::GOT_TLS_OFFSET
}
