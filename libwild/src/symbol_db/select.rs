use super::db::AtomicSymbolDb;
use super::db::SymbolDb;
use super::ids::SymbolId;
use crate::bail;
use crate::error::Error;
use crate::error::Result;
use crate::platform::Args;
use crate::platform::Platform;
use crate::platform::Symbol;
use crate::resolution::ResolvedGroup;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::FlagsForSymbol;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crossbeam_queue::SegQueue;
use hashbrown::HashMap;
use itertools::Itertools;
use rayon::iter::IntoParallelRefMutIterator as _;
use rayon::iter::ParallelIterator;
use std::mem::take;

/// For each symbol that has multiple definitions, some of which may be weak, some strong, some
/// "common" symbols and some in archive entries that weren't loaded, resolve which version of the
/// symbol we're using. The symbol we select will be the first strongly defined symbol in a loaded
/// object, or if there are no strong definitions, then the first definition in a loaded object. If
/// a symbol definition is a common symbol, then the largest definition will be used.
pub(crate) fn resolve_alternative_symbol_definitions<'data, P: Platform>(
    symbol_db: &mut SymbolDb<'data, P>,
    per_symbol_flags: &mut PerSymbolFlags,
    resolved: &[ResolvedGroup<'data, P>],
) -> Result {
    timing_phase!("Resolve alternative symbol definitions");

    let mut buckets = take(&mut symbol_db.buckets);
    let atomic_symbol_db = symbol_db.borrow_atomic();
    let atomic_per_symbol_flags = per_symbol_flags.borrow_atomic();
    let error_queue = SegQueue::new();

    buckets.par_iter_mut().for_each(|bucket| {
        verbose_timing_phase!("Resolve alternative for bucket");

        process_alternatives(
            &mut bucket.alternative_definitions,
            &error_queue,
            &atomic_symbol_db,
            &atomic_per_symbol_flags,
            resolved,
        );

        process_alternatives(
            &mut bucket.alternative_versioned_definitions,
            &error_queue,
            &atomic_symbol_db,
            &atomic_per_symbol_flags,
            resolved,
        );
    });

    drop(atomic_symbol_db);

    let mut duplicate_errors = error_queue.into_iter().collect_vec();
    duplicate_errors.sort_by_key(|e| e.to_string());

    if !duplicate_errors.is_empty() {
        let error_details = duplicate_errors
            .iter()
            .map(|e| e.to_string())
            .collect_vec()
            .join("\n");

        bail!("Duplicate symbols detected: {error_details}");
    }

    symbol_db.buckets = buckets;

    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Visibility {
    Default,
    Protected,
    Hidden,
}

fn process_alternatives<'data, P: Platform>(
    alternative_definitions: &mut HashMap<SymbolId, Vec<SymbolId>>,
    error_queue: &SegQueue<Error>,
    symbol_db: &AtomicSymbolDb<'data, '_, P>,
    per_symbol_flags: &AtomicPerSymbolFlags,
    resolved: &[ResolvedGroup<'data, P>],
) {
    for (first, alternatives) in std::mem::take(alternative_definitions) {
        // Compute the most restrictive visibility of any of the alternative definitions. This is
        // the visibility we'll use for our selected symbol. This seems like odd behaviour, but it
        // matches what GNU ld appears to do and some programs will fail to link if we don't do
        // this.
        let visibility = alternatives
            .iter()
            .fold(symbol_db.input_symbol_visibility(first), |vis, id| {
                vis.max(symbol_db.input_symbol_visibility(*id))
            });

        match select_symbol(symbol_db, per_symbol_flags, first, &alternatives, resolved) {
            Ok(selected) => {
                symbol_db.update_definition(first, selected);

                for &alt in &alternatives {
                    symbol_db.update_definition(alt, selected);
                }

                if visibility != Visibility::Default {
                    handle_non_default_visibility(per_symbol_flags, first, visibility);

                    for alt in alternatives {
                        handle_non_default_visibility(per_symbol_flags, alt, visibility);
                    }
                }
            }
            Err(err) => {
                error_queue.push(err);
            }
        }
    }
}

/// Update value flags for `symbol_id` given that we've now changed its visibility to something
/// other than default. Used during alternative resolution (parallel, atomic flags).
fn handle_non_default_visibility(
    per_symbol_flags: &AtomicPerSymbolFlags,
    symbol_id: SymbolId,
    visibility: Visibility,
) {
    let flags = per_symbol_flags.get_atomic(symbol_id);
    match visibility {
        Visibility::Hidden => {
            // Hidden merged visibility must localize the symbol so it cannot leak into dynsym.
            // However, symbols from shared libraries must not be downgraded since that would remove
            // them from the dynamic symbol table and prevent runtime resolution.
            if !flags.get().contains(ValueFlags::DYNAMIC) {
                flags.or_assign(ValueFlags::NON_INTERPOSABLE | ValueFlags::DOWNGRADE_TO_LOCAL);
            }
        }
        Visibility::Protected => {
            if !flags.get().contains(ValueFlags::DYNAMIC) {
                flags.or_assign(ValueFlags::NON_INTERPOSABLE);
            }
        }
        Visibility::Default => {}
    }
}

/// Applies visibility flags from a hidden/protected undefined reference to its definition.
/// Called during canonicalisation when we find the definition for an undefined symbol.
pub(crate) fn apply_visibility_to_definition(
    per_symbol_flags: &mut PerSymbolFlags,
    definition_id: SymbolId,
    visibility: Visibility,
) {
    match visibility {
        Visibility::Hidden => {
            per_symbol_flags.set_flag(
                definition_id,
                ValueFlags::NON_INTERPOSABLE | ValueFlags::DOWNGRADE_TO_LOCAL,
            );
        }
        Visibility::Protected => {
            if !per_symbol_flags
                .flags_for_symbol(definition_id)
                .contains(ValueFlags::DYNAMIC)
            {
                per_symbol_flags.set_flag(definition_id, ValueFlags::NON_INTERPOSABLE);
            }
        }
        Visibility::Default => {}
    }
}

/// Selects which version of the symbol to use. For more information on symbol priority, see
/// https://maskray.me/blog/2021-06-20-linker-symbol-resolution
#[inline(always)]
fn select_symbol<'data, P: Platform>(
    symbol_db: &AtomicSymbolDb<'data, '_, P>,
    per_symbol_flags: &AtomicPerSymbolFlags,
    first_id: SymbolId,
    alternatives: &[SymbolId],
    resolved: &[ResolvedGroup<'data, P>],
) -> Result<SymbolId> {
    let mut selector = SymbolPrioritySelector::new();

    for id in std::iter::once(first_id).chain(alternatives.iter().copied()) {
        let flags = per_symbol_flags.flags_for_symbol(id);

        // Dynamic symbols, even strong ones, don't override non-dynamic weak symbols, so in this
        // first pass, we ignore dynamic symbols.
        if flags.is_dynamic() {
            continue;
        }

        let strength = symbol_db.symbol_strength(id, resolved);

        // Check for duplicate strong definitions (COMDAT handling).
        if matches!(strength, SymbolStrength::Strong)
            && let Some(existing) = selector.first_strong
        {
            // We don't implement full COMDAT logic, however if we encounter duplicate
            // strong definitions, then we don't emit errors if all the strong definitions
            // are defined in COMDAT group sections.
            if (!symbol_db.is_in_comdat_group(existing, resolved)
                || !symbol_db.is_in_comdat_group(id, resolved))
                && !symbol_db.db.args.allow_multiple_definitions()
            {
                bail!(
                    "{}, defined in {} and {}",
                    symbol_db.symbol_name_for_display(first_id),
                    symbol_db.file(symbol_db.file_id_for_symbol(existing)),
                    symbol_db.file(symbol_db.file_id_for_symbol(id)),
                );
            }
        }

        selector.consider(id, strength);
    }

    if let Some(best) = selector.best() {
        return Ok(best);
    }

    // If we've made it this far, then the symbol is only defined in shared objects. Pick the first
    // definition. Note, we don't check for duplicate strong definitions here because it's OK for
    // multiple shared objects to define the same symbol strongly.
    for alt in std::iter::once(first_id).chain(alternatives.iter().copied()) {
        let strength = symbol_db.symbol_strength(alt, resolved);
        if strength != SymbolStrength::Undefined {
            return Ok(alt);
        }
    }

    Ok(first_id)
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum SymbolStrength {
    /// The object containing this symbol wasn't loaded, so the definition can be ignored.
    Undefined,

    /// The object weakly defines the symbol.
    Weak,

    /// The object uses STB_GNU_UNIQUE binding.
    GnuUnique,

    /// The object strongly defines the symbol.
    Strong,

    /// The symbol is a "common" symbol with the specified size. The definition with the largest
    /// size will be selected.
    Common(u64),
}

impl SymbolStrength {
    /// Computes the binding strength of a symbol from its attributes.
    pub(crate) fn of(symbol: &impl Symbol) -> Self {
        if symbol.is_weak() {
            SymbolStrength::Weak
        } else if symbol.is_common() {
            SymbolStrength::Common(symbol.size())
        } else if symbol.is_gnu_unique() {
            SymbolStrength::GnuUnique
        } else {
            SymbolStrength::Strong
        }
    }
}

/// Accumulates symbol candidates and selects the best one based on binding priority:
/// strong > common (largest) > weak/gnu_unique.
pub(crate) struct SymbolPrioritySelector {
    pub(crate) first_strong: Option<SymbolId>,
    max_common: Option<(u64, SymbolId)>,
    first_weak: Option<SymbolId>,
}

impl SymbolPrioritySelector {
    pub(crate) fn new() -> Self {
        Self {
            first_strong: None,
            max_common: None,
            first_weak: None,
        }
    }

    /// Consider a candidate symbol with the given strength.
    pub(crate) fn consider(&mut self, id: SymbolId, strength: SymbolStrength) {
        match strength {
            SymbolStrength::Strong => {
                if self.first_strong.is_none() {
                    self.first_strong = Some(id);
                }
            }
            SymbolStrength::Weak | SymbolStrength::GnuUnique => {
                if self.first_weak.is_none() {
                    self.first_weak = Some(id);
                }
            }
            SymbolStrength::Common(size) => match self.max_common {
                Some((prev_size, _)) if size <= prev_size => {}
                _ => self.max_common = Some((size, id)),
            },
            SymbolStrength::Undefined => {}
        }
    }

    /// Returns the best symbol based on priority: strong > common (largest) > weak.
    pub(crate) fn best(self) -> Option<SymbolId> {
        self.first_strong
            .or(self.max_common.map(|(_, id)| id))
            .or(self.first_weak)
    }
}

/// Returns whether the supplied symbol name is for a [mapping
/// symbol](https://github.com/ARM-software/abi-aa/blob/main/aaelf64/aaelf64.rst#mapping-symbols).
pub(crate) fn is_mapping_symbol_name(name: &[u8]) -> bool {
    name.starts_with(b"$x") || name.starts_with(b"$d") || name == b"L0\x01"
}
