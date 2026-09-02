use super::ids::*;
use super::load::SymbolVecWriters;
use super::load::linker_plugin_disabled_error;
use super::load::num_symbol_hash_buckets;
use super::load::populate_symbol_db;
use super::load::read_symbols;
use super::select::SymbolPrioritySelector;
use super::select::SymbolStrength;
use super::select::Visibility;
use super::select::is_mapping_symbol_name;
use crate::InputLinkerScript;
use crate::OutputKind;
use crate::bail;
use crate::error::Result;
use crate::export_list::ExportList;
use crate::grouping;
use crate::grouping::Group;
use crate::grouping::SequencedInput;
use crate::hash::PassThroughHashMap;
use crate::hash::PreHashed;
use crate::input_data::AuxiliaryFiles;
use crate::input_data::FileId;
use crate::input_data::LoadedInputs;
use crate::input_data::PRELUDE_FILE_ID;
use crate::layout_rules::LayoutRulesBuilder;
use crate::output_section_id::OutputSections;
use crate::parsing;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SyntheticSymbols;
use crate::part_id::PartId;
use crate::platform::Args;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionHeader;
use crate::platform::Symbol;
use crate::resolution::ResolvedFile;
use crate::resolution::ResolvedGroup;
use crate::resolution::ResolvedSyntheticSymbols;
use crate::sharding::ShardKey;
use crate::symbol::PreHashedSymbolName;
use crate::symbol::UnversionedSymbolName;
use crate::symbol::VersionedSymbolName;
use crate::timing_phase;
use crate::value_flags::FlagsForSymbol;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crate::version_script::RustVersionScript;
use crate::version_script::VersionScript;
use hashbrown::HashMap;
use hashbrown::hash_map;
use itertools::Itertools;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::IntoParallelRefMutIterator;
use rayon::iter::ParallelIterator;
use std::mem::take;
use symbolic_demangle::demangle;

#[derive(Debug)]
pub struct SymbolDb<'data, P: Platform> {
    pub(crate) args: &'data P::Args,

    pub(crate) groups: Vec<Group<'data, P>>,

    pub(super) buckets: Vec<SymbolBucket<'data>>,

    /// Which file each symbol ID belongs to.
    symbol_file_ids: Vec<FileId>,

    /// Mapping from symbol IDs to the canonical definition of that symbol. For global symbols that
    /// were selected as the definition and for all locals, this will point to itself. e.g. the
    /// value at index 5 will be the symbol ID 5.
    symbol_definitions: Vec<SymbolId>,

    /// The names of symbols that mark the start / stop of sections. These are indexed by the
    /// offset into the SyntheticSymbols' symbol IDs.
    start_stop_symbol_names: Vec<UnversionedSymbolName<'data>>,

    pub(crate) version_script: VersionScript<'data>,
    pub(crate) export_list: Option<ExportList<'data>>,

    /// The name of the entry symbol if overridden by a linker script.
    entry: Option<&'data [u8]>,

    pub(crate) output_kind: OutputKind,
    pub(crate) herd: &'data bumpalo_herd::Herd,

    /// The next input section ID to assign. Updated by `create_groups` so that subsequent calls
    /// (e.g. for LTO output objects) continue from where the previous call left off.
    pub(crate) next_input_section_id: crate::input_section_id::InputSectionId,

    /// Output part IDs for all input sections across all files, indexed by `InputSectionId`.
    /// Populated after section resolution and `assign_section_ids`. Note that for non-loaded
    /// sections, this will indicate the part that the section would have been placed in had it
    /// been loaded.
    pub(crate) section_part_ids: Vec<PartId>,

    /// Link-order assigned to plugin codegen objects so they are placed where the first LTO
    /// input appeared on the command line (#1935). SymbolIds stay at the end of the ID space.
    pub(crate) plugin_codegen_link_order: Option<u32>,
}

/// Borrows from a SymbolDb, but allows temporary atomic access to some of the tables. These tables
/// are returned to the original SymbolDb when the AtomicSymbolDb is dropped. If the AtomicSymbolDb
/// gets leaked, then the tables in the original SymbolDb will remain empty. Provides some, but not
/// all of the APIs provided by SymbolDb.
pub(super) struct AtomicSymbolDb<'data, 'db, P: Platform> {
    pub(super) db: &'db mut SymbolDb<'data, P>,
    definitions: Vec<AtomicSymbolId>,
}

#[derive(Debug)]
pub(super) struct SymbolBucket<'data> {
    /// Mapping from global symbol names to a symbol ID with that name. If there are multiple
    /// globals with the same name, then this will point to the one we encountered first, which may
    /// not be the selected definition. In order to find the selected definition, you still need to
    /// look at `symbol_definitions`.
    pub(super) name_to_id: PassThroughHashMap<UnversionedSymbolName<'data>, SymbolId>,

    pub(super) versioned_name_to_id: PassThroughHashMap<VersionedSymbolName<'data>, SymbolId>,

    /// Global symbols that have multiple definitions keyed by the first symbol with that name.
    pub(super) alternative_definitions: HashMap<SymbolId, Vec<SymbolId>>,

    /// Alternative definitions, but only for versioned symbols. This might be more efficient with
    /// a proper multi-map that doesn't need a separate Vec for each value, however we don't
    /// expect many entries here.
    pub(super) alternative_versioned_definitions: HashMap<SymbolId, Vec<SymbolId>>,
}

/// A global symbol that hasn't been put into our database yet.
#[derive(Clone, Copy)]
pub(super) struct PendingSymbol<'data> {
    pub(super) symbol_id: SymbolId,
    pub(super) name: PreHashed<UnversionedSymbolName<'data>>,
}

#[derive(Clone, Copy)]
pub(super) struct PendingVersionedSymbol<'data> {
    pub(super) symbol_id: SymbolId,
    pub(super) name: PreHashed<VersionedSymbolName<'data>>,
}
impl<'data, P: Platform> SymbolDb<'data, P> {
    /// If the version script is optimized fur rust, we downgraded all symbols to local visibility.
    /// This promotes symbols marked for global visibility in a Rust version script back to global.
    /// Also adds the non-interposable flag to all local symbols.
    pub(crate) fn handle_rust_version_script(
        &self,
        rust_vscript: &RustVersionScript<'data>,
        per_symbol_flags: &mut PerSymbolFlags,
    ) {
        verbose_timing_phase!("Upgrade locals for export");
        let atomic_per_symbol_flags = per_symbol_flags.borrow_atomic();

        rust_vscript.global.par_iter().for_each(|symbol| {
            let prehashed = UnversionedSymbolName::prehashed(symbol);
            if let Some(symbol_id) = self.get_unversioned(&prehashed) {
                atomic_per_symbol_flags
                    .get_atomic(self.definition(symbol_id))
                    .remove(ValueFlags::DOWNGRADE_TO_LOCAL);
            }
        });

        // Don't forget to add the non-interposable flag the local symbols.
        // We couldn't do this earlier as we didn't know which symbols would remain
        // local.
        per_symbol_flags
            .flags_mut()
            .par_iter_mut()
            .for_each(|flags| {
                let flags_val = flags.get();
                if flags_val.is_downgraded_to_local() {
                    *flags = (flags_val | ValueFlags::NON_INTERPOSABLE).raw();
                }
            });
    }

    pub(crate) fn new(
        args: &'data P::Args,
        output_kind: OutputKind,
        auxiliary: &AuxiliaryFiles<'data>,
        herd: &'data bumpalo_herd::Herd,
    ) -> Result<Self> {
        let version_script = auxiliary
            .version_script_data
            .map(VersionScript::parse)
            .transpose()?
            .unwrap_or_default();

        let export_list = auxiliary
            .export_list_data
            .map(ExportList::parse)
            .transpose()?;

        let num_buckets = num_symbol_hash_buckets(args);
        let mut buckets = Vec::new();
        buckets.resize_with(num_buckets, || SymbolBucket {
            name_to_id: Default::default(),
            versioned_name_to_id: Default::default(),
            alternative_definitions: HashMap::new(),
            alternative_versioned_definitions: HashMap::new(),
        });

        let mut symbol_db = SymbolDb {
            args,
            buckets,
            symbol_file_ids: Vec::new(),
            symbol_definitions: Vec::new(),
            groups: Vec::new(),
            start_stop_symbol_names: Default::default(),
            version_script,
            export_list,
            entry: None,
            output_kind,
            herd,
            section_part_ids: Vec::new(),
            next_input_section_id: crate::input_section_id::InputSectionId::from_usize(0),
            plugin_codegen_link_order: None,
        };

        for symbol in args.force_export_symbol_names() {
            symbol_db
                .export_list
                .get_or_insert_default()
                .add_symbol(symbol, true)?;
        }

        Ok(symbol_db)
    }

    pub(crate) fn add_inputs(
        &mut self,
        per_symbol_flags: &mut PerSymbolFlags,
        output_sections: &mut OutputSections<'data, P>,
        layout_rules_builder: &mut LayoutRulesBuilder<'data>,
        loaded: LoadedInputs<'data, P>,
    ) -> Result {
        timing_phase!("Load inputs into symbol DB");

        let parsed_objects = loaded.objects.into_iter().try_collect()?;

        let processed_linker_scripts = parsing::process_linker_scripts(
            &loaded.linker_scripts,
            output_sections,
            layout_rules_builder,
            self.args,
        )?;

        self.add_version_script_from_linker_scripts(&loaded.linker_scripts)?;

        let pre_existing_groups = self.groups.len();

        if self.groups.is_empty() {
            self.groups
                .push(Group::Prelude(crate::parsing::Prelude::new(
                    self.args,
                    self.output_kind,
                )?));
        }

        grouping::create_groups(
            self,
            parsed_objects,
            loaded.stub_libraries,
            processed_linker_scripts,
            loaded.objects_before_first_lto,
        );

        self.create_lto_input_groups(loaded.lto_objects)?;

        let new_groups = &self.groups[pre_existing_groups..];

        let num_symbols = new_groups.iter().map(|group| group.num_symbols()).sum();

        self.symbol_definitions.reserve(num_symbols);
        per_symbol_flags.reserve(num_symbols);
        self.symbol_file_ids.reserve(num_symbols);

        let mut writers = SymbolVecWriters::new(
            &mut self.symbol_definitions,
            &mut per_symbol_flags.flags,
            &mut self.symbol_file_ids,
        );

        let mut per_group_shards = new_groups
            .iter()
            .map(|group| writers.new_shard(group))
            .collect_vec();

        let per_group_outputs = read_symbols(
            &self.version_script,
            &mut per_group_shards,
            self.args,
            self.export_list.as_ref(),
            self.output_kind,
        )?;

        populate_symbol_db(&mut self.buckets, &per_group_outputs);

        {
            verbose_timing_phase!("Return shards");

            for shard in per_group_shards {
                writers.return_shard(shard);
            }
        }

        rayon::join(
            || {
                // This can take a while, so do it in parallel with other work.
                verbose_timing_phase!("Drop per-group outputs");
                drop(per_group_outputs);
            },
            || {
                verbose_timing_phase!("Apply linker scripts");

                for script in &loaded.linker_scripts {
                    self.apply_linker_script(script);
                }
            },
        );

        Ok(())
    }

    #[cfg(all(feature = "plugins", unix))]
    fn create_lto_input_groups(
        &mut self,
        lto_objects: Vec<Result<Box<crate::linker_plugins::LtoInputInfo<'data>>>>,
    ) -> Result {
        if lto_objects.is_empty() {
            return Ok(());
        }

        verbose_timing_phase!("Create LTO input groups");

        let lto_objects = lto_objects.into_iter().collect::<Result<Vec<_>>>()?;

        for group_objects in &lto_objects
            .into_iter()
            .chunks(crate::input_data::MAX_FILES_PER_GROUP as usize)
        {
            let mut next_symbol_id = self.next_symbol_id();
            let group_index = self.next_group_index();

            self.groups.push(Group::LtoInputs(
                group_objects
                    .into_iter()
                    .enumerate()
                    .map(|(file_index, o)| {
                        let symbol_id_range = SymbolIdRange::input(next_symbol_id, o.num_symbols());
                        let input_obj = o.into_input_object(
                            FileId::new(group_index, file_index as u32),
                            symbol_id_range,
                        );
                        next_symbol_id = next_symbol_id.add_usize(symbol_id_range.len());
                        input_obj
                    })
                    .collect(),
            ));
        }

        Ok(())
    }

    #[cfg(not(all(feature = "plugins", unix)))]
    #[allow(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        clippy::needless_pass_by_ref_mut
    )]
    fn create_lto_input_groups(
        &mut self,
        lto_objects: Vec<Result<Box<crate::linker_plugins::LtoInputInfo<'data>>>>,
    ) -> Result {
        if !lto_objects.is_empty() {
            return Err(linker_plugin_disabled_error());
        }
        Ok(())
    }

    /// Adds a new synthetic symbol. `syn` must have been the most recently added group.
    pub(crate) fn add_synthetic_symbol(
        &mut self,
        per_symbol_flags: &mut PerSymbolFlags,
        symbol_name: PreHashed<UnversionedSymbolName<'data>>,
        syn: &ResolvedSyntheticSymbols<'data, P>,
    ) -> SymbolId {
        debug_assert_eq!(syn.file_id.group() + 1, self.groups.len());

        let symbol_id = SymbolId::from_usize(self.symbol_definitions.len());

        debug_assert_eq!(
            symbol_id.0,
            syn.start_symbol_id.0 + syn.symbol_definitions.len() as u32
        );

        let num_buckets = self.buckets.len();
        self.buckets[symbol_name.hash() as usize % num_buckets].add_symbol(&PendingSymbol {
            symbol_id,
            name: symbol_name,
        });

        self.symbol_definitions.push(symbol_id);
        self.start_stop_symbol_names.push(*symbol_name);
        let Group::SyntheticSymbols(s) = &mut self.groups[syn.file_id.group()] else {
            panic!("Tried to add synthetic symbol to non-synthetic-symbol group");
        };
        s.symbol_id_range.num_symbols += 1;
        self.symbol_file_ids.push(syn.file_id);

        per_symbol_flags.push(ValueFlags::NON_INTERPOSABLE);

        symbol_id
    }

    /// Applies overrides for symbols wrapped via the --wrap= argument. Note that like GNU ld, our
    /// wrapping mechanism only affects resolution of undefined symbols. Defined symbols will be
    /// unaffected. This means that references to a symbol from within the compilation unit that
    /// defines it will not go via the wrapper. This is in contrast to LLD where wrapping also
    /// affects references to symbols in compilation units where those symbols are defined. Our main
    /// reason for this choice of behaviour is that it's much simpler to implement.
    pub(crate) fn apply_wrapped_symbol_overrides(&mut self) {
        let wrap = self.args.symbol_names_to_wrap();
        if wrap.is_empty() {
            return;
        }

        verbose_timing_phase!("Apply wrapped symbol overrides");

        let allocator = self.herd.get();

        for name in wrap {
            let name_bytes = allocator.alloc_slice_copy(name.as_bytes());
            let real_name = allocator.alloc_slice_copy(format!("__real_{name}").as_bytes());

            // When this function is called a second time (after LTO), the name table already has
            // "foo" mapped to __wrap_foo's symbol ID from the first call. To get the ORIGINAL foo
            // symbol ID, we first check if __real_foo already has a mapping (set by the first
            // call), and fall back to looking up "foo" only on the first call.
            let orig_id = self
                .get_unversioned(&UnversionedSymbolName::prehashed(real_name))
                .or_else(|| self.get_unversioned(&UnversionedSymbolName::prehashed(name_bytes)));

            let wrap_name = format!("__wrap_{name}");
            if let Some(wrap_id) =
                self.get_unversioned(&UnversionedSymbolName::prehashed(wrap_name.as_bytes()))
            {
                self.override_name(UnversionedSymbolName::prehashed(name_bytes), wrap_id);
            }

            if let Some(orig_id) = orig_id {
                self.override_name(UnversionedSymbolName::prehashed(real_name), orig_id);
            }
        }
    }

    /// Restores name-table entries for wrapped symbols to their original (pre-wrap) definitions.
    #[cfg(all(feature = "plugins", unix))]
    pub(crate) fn restore_wrapped_symbol_names(&mut self) {
        let wrap = self.args.symbol_names_to_wrap();
        if wrap.is_empty() {
            return;
        }

        let allocator = self.herd.get();

        for name in wrap {
            let name_bytes = allocator.alloc_slice_copy(name.as_bytes());
            let real_name = allocator.alloc_slice_copy(format!("__real_{name}").as_bytes());

            if let Some(orig_id) =
                self.get_unversioned(&UnversionedSymbolName::prehashed(real_name))
            {
                self.override_name(UnversionedSymbolName::prehashed(name_bytes), orig_id);
            }
        }
    }

    /// Overrides `name` to point to `symbol_id`. Returns the old symbol ID for `name`.
    fn override_name(
        &mut self,
        name: PreHashed<UnversionedSymbolName<'data>>,
        symbol_id: SymbolId,
    ) -> Option<SymbolId> {
        let num_buckets = self.buckets.len();
        self.buckets[name.hash() as usize % num_buckets]
            .name_to_id
            .insert(name, symbol_id)
    }

    /// Reads the symbol visibility from the original object.
    pub(crate) fn input_symbol_visibility(&self, symbol_id: SymbolId) -> Visibility {
        let file_id = self.file_id_for_symbol(symbol_id);
        debug_assert!(self.file(file_id).symbol_id_range().contains(symbol_id));
        match &self.groups[file_id.group()] {
            Group::Prelude(_) => Visibility::Default,
            Group::Objects(parsed_input_objects) => {
                let obj = &parsed_input_objects[file_id.file()];
                let local_index = symbol_id.to_input(obj.symbol_id_range);

                let Ok(obj_symbol) = obj.parsed.object.symbol(local_index) else {
                    return Visibility::Default;
                };

                obj_symbol.visibility()
            }
            Group::StubLibraries(_) => Visibility::Default,
            Group::LinkerScripts(_) => Visibility::Default,
            Group::SyntheticSymbols(_) => Visibility::Default,
            #[cfg(all(feature = "plugins", unix))]
            Group::LtoInputs(lto_objects) => {
                lto_objects[file_id.file()].symbol_visibility(symbol_id)
            }
        }
    }

    /// Returns a struct that can be used to print debug information about the specified symbol.
    pub(crate) fn symbol_debug<'a>(
        &'a self,
        per_symbol_flags: &'a dyn FlagsForSymbol,
        symbol_id: SymbolId,
    ) -> SymbolDebug<'a, 'data, P> {
        SymbolDebug {
            db: self,
            symbol_id,
            per_symbol_flags,
        }
    }

    pub(crate) fn symbol_name_for_display(&self, symbol_id: SymbolId) -> SymbolNameDisplay<'data> {
        SymbolNameDisplay {
            name: self.symbol_name(symbol_id).ok(),
            demangle: self.args.common().demangle,
        }
    }

    pub(crate) fn symbol_name(&self, symbol_id: SymbolId) -> Result<UnversionedSymbolName<'data>> {
        let file_id = self.file_id_for_symbol(symbol_id);
        match &self.groups[file_id.group()] {
            Group::Prelude(prelude) => Ok(prelude.symbol_name(symbol_id)),
            Group::Objects(parsed_input_objects) => {
                parsed_input_objects[file_id.file()].symbol_name(symbol_id)
            }
            Group::StubLibraries(stubs) => Ok(stubs[file_id.file()].symbol_name(symbol_id)),
            Group::LinkerScripts(scripts) => Ok(scripts[file_id.file()].symbol_name(symbol_id)),
            Group::SyntheticSymbols(syn) => {
                Ok(self.start_stop_symbol_names[syn.symbol_id_range.id_to_offset(symbol_id)])
            }
            #[cfg(all(feature = "plugins", unix))]
            Group::LtoInputs(lto_objects) => Ok(lto_objects[file_id.file()].symbol_name(symbol_id)),
        }
    }

    /// Returns the prelude definition for `symbol_id` when it belongs to the prelude.
    pub(crate) fn prelude_symbol_def(
        &self,
        symbol_id: SymbolId,
    ) -> Option<&InternalSymDefInfo<'data, P>> {
        let file_id = self.file_id_for_symbol(symbol_id);
        if file_id != PRELUDE_FILE_ID {
            return None;
        }
        match &self.groups[file_id.group()] {
            Group::Prelude(prelude) => Some(prelude.symbol_def(symbol_id)),
            _ => None,
        }
    }

    /// Get the version of a symbol. Only intended for diagnostic purposes.
    pub(crate) fn symbol_version_debug(&self, symbol_id: SymbolId) -> Option<String> {
        let file_id = self.file_id_for_symbol(symbol_id);
        match &self.groups[file_id.group()] {
            Group::Objects(parsed_input_objects) => {
                parsed_input_objects[file_id.file()].symbol_version_debug(symbol_id)
            }
            _ => None,
        }
    }

    pub(crate) fn flags_for_symbol(
        &self,
        per_symbol_flags: &PerSymbolFlags,
        symbol_id: SymbolId,
    ) -> ValueFlags {
        let mut flags = per_symbol_flags.flags_for_symbol(self.definition(symbol_id));
        flags.merge(per_symbol_flags.flags_for_symbol(symbol_id));
        flags
    }

    pub(crate) fn num_symbols(&self) -> usize {
        self.symbol_definitions.len()
    }

    pub(crate) fn num_regular_objects(&self) -> usize {
        self.groups
            .iter()
            .map(|group| match group {
                Group::Objects(objects) => objects.len(),
                _ => 0,
            })
            .sum()
    }

    pub(crate) fn num_lto_objects(&self) -> usize {
        self.groups
            .iter()
            .map(|group| match group {
                #[cfg(all(feature = "plugins", unix))]
                Group::LtoInputs(objects) => objects.len(),
                _ => 0,
            })
            .sum()
    }

    /// If we have a symbol that when demangled produces `target_name`, then return the mangled
    /// name. Note, this scans every symbol, so should only be used for debugging / diagnostic
    /// purposes.
    pub(crate) fn find_mangled_name(&self, target_name: &str) -> Option<String> {
        for i in 1..self.num_symbols() {
            let symbol_id = SymbolId(i as u32);
            let Ok(name) = self.symbol_name(symbol_id) else {
                continue;
            };

            let Ok(name) = std::str::from_utf8(name.bytes()) else {
                continue;
            };

            if demangle(name) == target_name {
                return Some(name.to_owned());
            }
        }

        None
    }

    /// Returns our mapping from symbol IDs to the IDs that define them. Definitions should be
    /// restored later by calling `restore_definitions`. While the definitions are taken, any method
    /// that requires definitions will fail.
    pub(crate) fn take_definitions(&mut self) -> Vec<SymbolId> {
        take(&mut self.symbol_definitions)
    }

    pub(crate) fn restore_definitions(&mut self, definitions: Vec<SymbolId>) {
        self.symbol_definitions = definitions;
    }

    pub(super) fn borrow_atomic<'db>(&'db mut self) -> AtomicSymbolDb<'data, 'db, P> {
        let definitions = self
            .take_definitions()
            .into_iter()
            .map(|id| id.as_atomic())
            .collect();

        AtomicSymbolDb {
            db: self,
            definitions,
        }
    }

    pub(crate) fn file_id_for_symbol(&self, symbol_id: SymbolId) -> FileId {
        self.symbol_file_ids[symbol_id.as_usize()]
    }

    /// Returns whether the supplied symbol ID is the canonical ID. A symbol won't be canonical, if
    /// it resolves to a different symbol. The symbol may still be undefined.
    pub(crate) fn is_canonical(&self, symbol_id: SymbolId) -> bool {
        let resolution = self.symbol_definitions[symbol_id.as_usize()];
        resolution == symbol_id
    }

    pub(crate) fn definition(&self, symbol_id: SymbolId) -> SymbolId {
        // We need to do two steps when finding the definition for a symbol, since the definition
        // may have changed since we did the original name lookup. It would be possible to avoid
        // this, by resolving all definitions before we resolve references, except then, due to
        // archive semantics, we'd need to do two passes to resolve symbols, one to determine which
        // archive members to load, then a second to determine which symbols to use.
        let step1 = self.symbol_definitions[symbol_id.as_usize()];
        self.symbol_definitions[step1.as_usize()]
    }

    pub(crate) fn replace_definition(&mut self, symbol_id: SymbolId, new_definition: SymbolId) {
        self.symbol_definitions[symbol_id.as_usize()] = new_definition;
    }

    pub(crate) fn file<'db>(&'db self, file_id: FileId) -> SequencedInput<'db, 'data, P> {
        match &self.groups[file_id.group()] {
            Group::Prelude(prelude) => SequencedInput::Prelude(prelude),
            Group::Objects(parsed_input_objects) => {
                SequencedInput::Object(&parsed_input_objects[file_id.file()])
            }
            Group::StubLibraries(stubs) => SequencedInput::StubLibrary(&stubs[file_id.file()]),
            Group::LinkerScripts(scripts) => SequencedInput::LinkerScript(&scripts[file_id.file()]),
            Group::SyntheticSymbols(syn) => SequencedInput::SyntheticSymbols(syn),
            #[cfg(all(feature = "plugins", unix))]
            Group::LtoInputs(lto_objects) => SequencedInput::LtoInput(&lto_objects[file_id.file()]),
        }
    }

    pub(crate) fn is_mapping_symbol(&self, symbol_id: SymbolId) -> bool {
        let Ok(name) = self.symbol_name(symbol_id) else {
            // We don't want to bother the caller with an error here. If there's a problem getting
            // the name, it will be reported elsewhere.
            return false;
        };
        is_mapping_symbol_name(name.bytes())
    }

    pub(crate) fn get_unversioned(
        &self,
        prehashed: &PreHashed<UnversionedSymbolName>,
    ) -> Option<SymbolId> {
        let num_buckets = self.buckets.len();
        self.buckets[prehashed.hash() as usize % num_buckets]
            .name_to_id
            .get(prehashed)
            .copied()
    }

    #[inline(always)]
    pub(crate) fn get(&self, key: &PreHashedSymbolName, allow_dynamic: bool) -> Option<SymbolId> {
        let num_buckets = self.buckets.len();

        match key {
            PreHashedSymbolName::Unversioned(key) => {
                let bucket = &self.buckets[key.hash() as usize % num_buckets];
                let symbol_id = bucket.name_to_id.get(key).copied()?;

                if !allow_dynamic && self.file(self.file_id_for_symbol(symbol_id)).is_dynamic() {
                    return bucket.get_non_dynamic(symbol_id, self);
                }

                Some(symbol_id)
            }
            PreHashedSymbolName::Versioned(key) => {
                let bucket = &self.buckets[key.hash() as usize % num_buckets];
                let symbol_id = bucket.versioned_name_to_id.get(key).copied()?;

                if !allow_dynamic && self.file(self.file_id_for_symbol(symbol_id)).is_dynamic() {
                    return bucket.get_non_dynamic(symbol_id, self);
                }

                Some(symbol_id)
            }
        }
    }

    pub(crate) fn all_unversioned_symbols(
        &self,
    ) -> impl Iterator<Item = (&PreHashed<UnversionedSymbolName<'data>>, &SymbolId)> {
        self.buckets.iter().flat_map(|b| b.name_to_id.iter())
    }

    #[inline(always)]
    pub(crate) fn symbol_strength(
        &self,
        symbol_id: SymbolId,
        resolved: &[ResolvedGroup<'data, P>],
    ) -> SymbolStrength {
        let file_id = self.file_id_for_symbol(symbol_id);
        match &resolved[file_id.group()].files[file_id.file()] {
            ResolvedFile::Object(obj) => obj.common.symbol_strength(symbol_id),
            ResolvedFile::Dynamic(obj) => obj.common.symbol_strength(symbol_id),
            ResolvedFile::StubLibrary(stub) => stub.symbol_strength(symbol_id),
            #[cfg(all(feature = "plugins", unix))]
            ResolvedFile::LtoInput(obj) => {
                use crate::linker_plugins::SymbolKind;

                let SequencedInput::LtoInput(obj) = self.file(obj.file_id) else {
                    unreachable!();
                };
                if !obj.enabled {
                    return SymbolStrength::Undefined;
                }
                let local_index = symbol_id.to_input(obj.symbol_id_range);
                let obj_symbol = &obj.symbols[local_index.0];
                match obj_symbol.kind {
                    Some(SymbolKind::Def) => SymbolStrength::Strong,
                    Some(SymbolKind::WeakDef) => SymbolStrength::Weak,
                    Some(SymbolKind::Common) => SymbolStrength::Common(obj_symbol.size),
                    _ => SymbolStrength::Undefined,
                }
            }
            _ => SymbolStrength::Undefined,
        }
    }

    /// Returns whether the specified symbol is defined in a section with the SHF_GROUP flag set.
    pub(super) fn is_in_comdat_group(
        &self,
        symbol_id: SymbolId,
        resolved: &[ResolvedGroup<'data, P>],
    ) -> bool {
        let file_id = self.file_id_for_symbol(symbol_id);
        let ResolvedFile::Object(obj) = &resolved[file_id.group()].files[file_id.file()] else {
            return false;
        };

        let local_index = symbol_id.to_input(obj.common.symbol_id_range);
        let Ok(obj_symbol) = obj.common.object.symbol(local_index) else {
            return false;
        };
        let Ok(Some(section_index)) = obj.common.object.symbol_section(obj_symbol, local_index)
        else {
            return false;
        };
        let Ok(header) = obj.common.object.section(section_index) else {
            return false;
        };

        header.is_group()
    }

    pub(crate) fn entry_point(&self) -> crate::platform::EntryPoint<'_> {
        self.args.entry_point(self.entry)
    }

    pub(crate) fn entry_symbol_name(&self) -> Option<&[u8]> {
        match self.entry_point() {
            crate::platform::EntryPoint::Symbol(name) => Some(name),
            crate::platform::EntryPoint::None | crate::platform::EntryPoint::Address(_) => None,
        }
    }

    fn apply_linker_script(&mut self, script: &InputLinkerScript<'data>) {
        for cmd in &script.script.commands {
            if let crate::linker_script::Command::Entry(symbol_name) = cmd {
                self.entry = Some(*symbol_name);
            }
        }
    }

    pub(crate) fn next_symbol_id(&self) -> SymbolId {
        self.groups.last().map_or(SymbolId::undefined(), |group| {
            let range = group.symbol_id_range();
            range.start().add_usize(range.len())
        })
    }

    pub(crate) fn new_synthetic_symbols_group(&mut self) -> ResolvedSyntheticSymbols<'data, P> {
        let file_id = FileId::new(self.groups.len() as u32, 0);
        let start_symbol_id = self.next_symbol_id();

        self.groups.push(Group::SyntheticSymbols(SyntheticSymbols {
            file_id,
            symbol_id_range: SymbolIdRange::input(start_symbol_id, 0),
        }));

        ResolvedSyntheticSymbols {
            file_id,
            start_symbol_id,
            symbol_definitions: Vec::new(),
        }
    }

    fn add_version_script_from_linker_scripts(
        &mut self,
        linker_scripts: &[InputLinkerScript<'data>],
    ) -> Result {
        for script in linker_scripts {
            // Check if the linker script contains a VERSION command
            if let Some(version_content) = script.script.get_version_script_content() {
                if self.version_script != VersionScript::default() {
                    bail!("Multiple version scripts provided");
                }

                self.version_script = VersionScript::parse(crate::input_data::ScriptData {
                    raw: version_content,
                })?;
            }
        }

        Ok(())
    }

    pub(crate) fn groups_reserve(&mut self, additional: usize) {
        self.groups.reserve(additional);
    }

    pub(crate) fn next_group_index(&self) -> u32 {
        self.groups.len() as u32
    }

    pub(crate) fn add_group(&mut self, group: Group<'data, P>) {
        self.groups.push(group);
    }

    #[allow(dead_code)]
    fn remap_symbol_file_ids_from(&mut self, from_group: usize, delta: u32) {
        for file_id in &mut self.symbol_file_ids {
            if file_id.group() >= from_group {
                *file_id = FileId::new(file_id.group() as u32 + delta, file_id.file() as u32);
            }
        }
    }

    #[cfg(all(feature = "plugins", unix))]
    pub(crate) fn disable_lto_inputs(&mut self) {
        for group in &mut self.groups {
            if let Group::LtoInputs(objects) = group {
                for obj in objects {
                    obj.enabled = false;
                }
            }
        }
    }

    pub(crate) fn is_undefined(&self, symbol_id: SymbolId) -> bool {
        let file_id = self.file_id_for_symbol(symbol_id);
        match &self.groups[file_id.group()] {
            Group::Objects(objects) => {
                let file = &objects[file_id.file()];

                let local_index = file.symbol_id_range.id_to_input(symbol_id);
                file.parsed
                    .object
                    .symbol(local_index)
                    .is_ok_and(|sym| sym.is_undefined())
            }
            // For symbols originating from linker scripts, prelude etc, we currently assume they're
            // all definitions.
            _ => false,
        }
    }

    pub(crate) fn warning(&self, message: impl Into<String>) {
        self.args.warning(message);
    }

    pub(crate) fn part_id_for_symbol(&self, symbol_id: SymbolId) -> PartId {
        let file_id = self.file_id_for_symbol(symbol_id);
        let file = self.file(file_id);
        if file.is_dynamic() {
            return crate::part_id::UNMAPPED;
        }
        let Some(input_section_id) = file.input_section_id_for_symbol(symbol_id) else {
            return crate::part_id::UNMAPPED;
        };
        self.section_part_ids[input_section_id.as_usize()]
    }
}
impl<'data> SymbolBucket<'data> {
    pub(super) fn add_symbol(&mut self, pending: &PendingSymbol<'data>) {
        match self.name_to_id.entry(pending.name) {
            hash_map::Entry::Occupied(entry) => {
                let first_symbol_id = *entry.get();
                self.add_extra_symbol_definition(first_symbol_id, pending.symbol_id);
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(pending.symbol_id);
            }
        }
    }

    pub(super) fn add_versioned_symbol(&mut self, pending: &PendingVersionedSymbol<'data>) {
        match self.versioned_name_to_id.entry(pending.name) {
            hash_map::Entry::Occupied(entry) => {
                let first_symbol_id = *entry.get();
                self.alternative_versioned_definitions
                    .entry(first_symbol_id)
                    .or_default()
                    .push(pending.symbol_id);
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(pending.symbol_id);
            }
        }
    }

    fn add_extra_symbol_definition(&mut self, first_symbol_id: SymbolId, new_symbol_id: SymbolId) {
        self.alternative_definitions
            .entry(first_symbol_id)
            .or_default()
            .push(new_symbol_id);
    }

    /// Returns the selected non-dynamic alternative to the supplied symbol, if any.
    /// Among non-dynamic alternatives, selects the best one based on symbol binding:
    /// strong > common (largest) > weak/gnu_unique.
    pub(super) fn get_non_dynamic<P: Platform>(
        &self,
        symbol_id: SymbolId,
        symbol_db: &SymbolDb<'data, P>,
    ) -> Option<SymbolId> {
        let alternatives = self.alternative_definitions.get(&symbol_id)?;
        let mut selector = SymbolPrioritySelector::new();
        for &alt in alternatives {
            let file_id = symbol_db.file_id_for_symbol(alt);
            let file = symbol_db.file(file_id);
            if file.is_dynamic() {
                continue;
            }
            selector.consider(alt, file.symbol_strength(alt));
        }
        selector.best()
    }
}
#[derive(Clone, Copy)]
pub(crate) struct SymbolDebug<'a, 'data, P: Platform> {
    db: &'a SymbolDb<'data, P>,
    symbol_id: SymbolId,
    per_symbol_flags: &'a dyn FlagsForSymbol,
}

impl<'a, 'data, P: Platform> std::fmt::Display for SymbolDebug<'a, 'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let symbol_id = self.symbol_id;
        let definition = self.db.definition(symbol_id);
        let file_id = self.db.file_id_for_symbol(symbol_id);
        let file = self.db.file(file_id);
        let symbol_id_range = file.symbol_id_range();

        if !symbol_id_range.contains(symbol_id) {
            write!(
                f,
                "SymbolId {symbol_id} is owned by {file_id}, but that file has range {}..{}",
                symbol_id_range.start(),
                symbol_id_range.start().add_usize(symbol_id_range.len())
            )?;
            // If ID ranges or file mappings are wrong, then the code later in this method, e.g.
            // `id_to_offset` or `symbol_name` will panic.
            return Ok(());
        }

        let local_index = symbol_id.to_offset(symbol_id_range);
        let symbol_name = self
            .db
            .symbol_name(symbol_id)
            .unwrap_or_else(|_| UnversionedSymbolName::new(b"??"));

        if definition.is_undefined() {
            write!(f, "undefined ")?;
        }

        if symbol_name.bytes().is_empty() {
            match file {
                SequencedInput::Prelude(_) => write!(f, "<unnamed internal symbol>")?,
                SequencedInput::Object(o) => {
                    let symbol_index = symbol_id.to_input(symbol_id_range);
                    if let Some(section_name) = o
                        .parsed
                        .object
                        .symbol(symbol_index)
                        .ok()
                        .and_then(|symbol| {
                            o.parsed
                                .object
                                .symbol_section(symbol, symbol_index)
                                .ok()
                                .flatten()
                        })
                        .map(|section_index| o.parsed.object.section_display_name(section_index))
                    {
                        write!(f, "section `{section_name}`")?;
                    } else {
                        write!(f, "<unnamed symbol>")?;
                    }
                }
                SequencedInput::StubLibrary(s) => {
                    write!(f, "<unnamed Mach-O stub library symbol from `{}`>", s.input)?;
                }
                SequencedInput::LinkerScript(s) => {
                    write!(f, "Symbol from linker script `{}`", s.parsed.input)?;
                }
                SequencedInput::SyntheticSymbols(_) => {
                    write!(f, "<unnamed custom-section symbol>")?;
                }
                #[cfg(all(feature = "plugins", unix))]
                SequencedInput::LtoInput(_) => write!(f, "<unnamed symbol from LTO object>")?,
            }
        } else {
            write!(f, "symbol `{}`", self.db.symbol_name_for_display(symbol_id))?;
        }

        write!(
            f,
            " ({symbol_id} local={local_index}) in file #{file_id} ({file})"
        )?;

        if symbol_id != definition && !definition.is_undefined() {
            let definition_file_id = self.db.file_id_for_symbol(definition);
            let definition_file = self.db.file(definition_file_id);
            write!(
                f,
                " defined as {definition} in file #{definition_file_id} ({definition_file})"
            )?;
        }

        let flags = self.per_symbol_flags.flags_for_symbol(symbol_id);
        write!(f, " ({flags})")?;

        Ok(())
    }
}
impl<'data> PendingSymbol<'data> {
    pub(super) fn new(symbol_id: SymbolId, name: &'data [u8]) -> PendingSymbol<'data> {
        Self::from_prehashed(symbol_id, UnversionedSymbolName::prehashed(name))
    }

    pub(super) fn from_prehashed(
        symbol_id: SymbolId,
        name: PreHashed<UnversionedSymbolName<'data>>,
    ) -> PendingSymbol<'data> {
        PendingSymbol { symbol_id, name }
    }
}

impl<'data> PendingVersionedSymbol<'data> {
    pub(super) fn from_prehashed(
        symbol_id: SymbolId,
        name: PreHashed<UnversionedSymbolName<'data>>,
        version: &'data [u8],
    ) -> PendingVersionedSymbol<'data> {
        PendingVersionedSymbol {
            symbol_id,
            name: VersionedSymbolName::prehashed(name, version),
        }
    }
}
impl<'data, 'db, P: Platform> AtomicSymbolDb<'data, 'db, P> {
    pub(super) fn input_symbol_visibility(&self, symbol_id: SymbolId) -> Visibility {
        self.db.input_symbol_visibility(symbol_id)
    }

    pub(super) fn update_definition(&self, to_update: SymbolId, new_definition: SymbolId) {
        self.definitions[to_update.as_usize()].store(new_definition);
    }

    pub(super) fn symbol_strength(
        &self,
        symbol_id: SymbolId,
        resolved: &[ResolvedGroup<'data, P>],
    ) -> SymbolStrength {
        self.db.symbol_strength(symbol_id, resolved)
    }

    pub(super) fn is_in_comdat_group(
        &self,
        symbol_id: SymbolId,
        resolved: &[ResolvedGroup<'data, P>],
    ) -> bool {
        self.db.is_in_comdat_group(symbol_id, resolved)
    }

    pub(super) fn symbol_name_for_display(&self, symbol_id: SymbolId) -> SymbolNameDisplay<'data> {
        self.db.symbol_name_for_display(symbol_id)
    }

    pub(super) fn file(&'db self, file_id: FileId) -> SequencedInput<'db, 'data, P> {
        self.db.file(file_id)
    }

    pub(super) fn file_id_for_symbol(&self, symbol_id: SymbolId) -> FileId {
        self.db.file_id_for_symbol(symbol_id)
    }
}

impl<'data, P: Platform> Drop for AtomicSymbolDb<'data, '_, P> {
    fn drop(&mut self) {
        // Convert our atomic tables back to non-atomic tables and return them to the symbol-db that
        // we took them from. This operation should be basically free, at least in optimised builds.
        self.db.restore_definitions(
            take(&mut self.definitions)
                .into_iter()
                .map(|id| id.into_non_atomic())
                .collect(),
        );
    }
}
