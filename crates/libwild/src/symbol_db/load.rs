use super::db::PendingSymbol;
use super::db::PendingVersionedSymbol;
use super::db::SymbolBucket;
use super::ids::*;
use crate::OutputKind;
use crate::error;
use crate::error::Context as _;
use crate::error::Error;
use crate::error::Result;
use crate::export_list::ExportList;
use crate::grouping::Group;
use crate::grouping::SequencedInputObject;
use crate::grouping::SequencedLinkerScript;
use crate::hash::PreHashed;
use crate::hash::hash_bytes;
use crate::input_data::FileId;
use crate::input_data::PRELUDE_FILE_ID;
use crate::layout::EnginePlatform;
use crate::output_section_id::OutputSectionId;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::Prelude;
use crate::parsing::Redirect;
use crate::parsing::SymbolLoc;
use crate::parsing::SymbolPlacement;
use crate::platform;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::RawSymbolName as _;
use crate::platform::Symbol;
use crate::symbol::UnversionedSymbolName;
use crate::timing_phase;
use crate::value_flags::RawFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crate::version_script::VersionScript;
use rayon::iter::IndexedParallelIterator;
use rayon::iter::IntoParallelRefMutIterator as _;
use rayon::iter::ParallelIterator;

pub(super) struct SymbolLoadOutputs<'data> {
    /// Pending non-versioned symbols, grouped by hash bucket.
    pending_symbols_by_bucket: Vec<PendingSymbolHashBucket<'data>>,
}

#[derive(Default, Clone)]
struct PendingSymbolHashBucket<'data> {
    symbols: Vec<PendingSymbol<'data>>,

    versioned_symbols: Vec<PendingVersionedSymbol<'data>>,
}
pub(crate) fn linker_plugin_disabled_error() -> Error {
    error!("Wild was compiled without linker-plugin support, but LTO inputs were detected")
}

pub(super) struct SymbolVecWriters<'out> {
    symbol_definitions_writer: sharded_vec_writer::VecWriter<'out, SymbolId>,
    per_symbol_flags_writer: sharded_vec_writer::VecWriter<'out, RawFlags>,
    symbol_file_ids_writer: sharded_vec_writer::VecWriter<'out, FileId>,
}

impl<'out> SymbolVecWriters<'out> {
    pub(super) fn new(
        symbol_definitions: &'out mut Vec<SymbolId>,
        per_symbol_flags: &'out mut Vec<RawFlags>,
        symbol_file_ids: &'out mut Vec<FileId>,
    ) -> Self {
        Self {
            symbol_definitions_writer: sharded_vec_writer::VecWriter::new(symbol_definitions),
            per_symbol_flags_writer: sharded_vec_writer::VecWriter::new(per_symbol_flags),
            symbol_file_ids_writer: sharded_vec_writer::VecWriter::new(symbol_file_ids),
        }
    }

    pub(super) fn new_shard<'group, 'data, P: EnginePlatform>(
        &mut self,
        group: &'group Group<'data, P>,
    ) -> SymbolWriterShard<'out, 'group, 'data, P> {
        let num_symbols = group.num_symbols();
        SymbolWriterShard {
            group,
            next: group.start_symbol_id(),
            resolutions: self.symbol_definitions_writer.take_shard(num_symbols),
            flags: self.per_symbol_flags_writer.take_shard(num_symbols),
            file_ids: self.symbol_file_ids_writer.take_shard(num_symbols),
        }
    }

    pub(super) fn return_shard<'data, P: EnginePlatform>(
        &mut self,
        shard: SymbolWriterShard<'_, '_, 'data, P>,
    ) {
        self.symbol_definitions_writer
            .return_shard(shard.resolutions);
        self.per_symbol_flags_writer.return_shard(shard.flags);
        self.symbol_file_ids_writer.return_shard(shard.file_ids);
    }
}
pub(super) fn read_symbols<'data, P: EnginePlatform>(
    version_script: &VersionScript,
    shards: &mut [SymbolWriterShard<'_, '_, 'data, P>],
    args: &P::Args,
    export_list: Option<&ExportList<'data>>,
    output_kind: OutputKind,
) -> Result<Vec<SymbolLoadOutputs<'data>>> {
    timing_phase!("Read symbols");

    let num_buckets = num_symbol_hash_buckets(args);

    shards
        .par_iter_mut()
        .map(|shard| {
            read_symbols_for_group(
                shard,
                version_script,
                export_list,
                num_buckets,
                args,
                output_kind,
            )
        })
        .collect::<Result<Vec<SymbolLoadOutputs>>>()
}

fn read_symbols_for_group<'data, P: EnginePlatform>(
    shard: &mut SymbolWriterShard<'_, '_, 'data, P>,
    version_script: &VersionScript,
    export_list: Option<&ExportList<'data>>,
    num_buckets: usize,
    args: &P::Args,
    output_kind: OutputKind,
) -> Result<SymbolLoadOutputs<'data>> {
    verbose_timing_phase!(
        "Read group symbols",
        group_id = shard.group.group_id(),
        num_symbols = shard.group.num_symbols()
    );

    let mut outputs = SymbolLoadOutputs {
        pending_symbols_by_bucket: vec![PendingSymbolHashBucket::default(); num_buckets],
    };

    match shard.group {
        Group::Prelude(prelude) => {
            prelude.load_symbols(shard, &mut outputs);
        }
        Group::Objects(parsed_input_objects) => {
            for obj in *parsed_input_objects {
                load_symbols_from_file(
                    obj,
                    version_script,
                    shard,
                    &mut outputs,
                    args,
                    export_list,
                    output_kind,
                )
                .with_context(|| format!("Failed to load symbols from `{}`", obj.parsed.input))?;
            }
        }
        Group::StubLibraries(stubs) => {
            for stub in stubs {
                load_stub_library_symbols(stub, shard, &mut outputs);
            }
        }
        Group::LinkerScripts(scripts) => {
            for script in scripts {
                load_linker_script_symbols(script, shard, &mut outputs);
            }
        }
        Group::SyntheticSymbols(_) => {
            // Custom section start/stop symbols are generated after archive handling.
        }
        #[cfg(all(feature = "plugins", unix))]
        Group::LtoInputs(lto_objects) => {
            for obj in lto_objects {
                load_lto_symbols(shard, &mut outputs, obj);
            }
        }
    }

    Ok(outputs)
}

fn load_stub_library_symbols<'data, P: EnginePlatform>(
    stub: &crate::grouping::SequencedStubLibrary<'data>,
    symbols_out: &mut SymbolWriterShard<'_, '_, 'data, P>,
    outputs: &mut SymbolLoadOutputs<'data>,
) {
    for (offset, symbol_name) in stub
        .defined_symbols
        .symbols
        .iter()
        .chain(stub.defined_symbols.weak_symbols.iter())
        .enumerate()
    {
        let symbol_id = stub.symbol_id_range.offset_to_id(offset);
        outputs.add_non_versioned(PendingSymbol::new(symbol_id, symbol_name.as_bytes()));
        symbols_out.set_next(ValueFlags::DYNAMIC, symbol_id, stub.file_id);
    }
}

#[cfg(all(feature = "plugins", unix))]
fn load_lto_symbols<'data, P: EnginePlatform>(
    symbols_out: &mut SymbolWriterShard<'_, '_, 'data, P>,
    outputs: &mut SymbolLoadOutputs<'data>,
    obj: &crate::linker_plugins::LtoInput<'data>,
) {
    for (symbol_id, sym) in obj.symbols_iter() {
        if sym.is_definition() {
            if let Some(version) = sym.version {
                outputs.add_versioned(PendingVersionedSymbol::from_prehashed(
                    symbol_id,
                    UnversionedSymbolName::prehashed(sym.name.bytes()),
                    version,
                ));
            } else {
                outputs.add_non_versioned(PendingSymbol::new(symbol_id, sym.name.bytes()));
            }
            symbols_out.set_next(ValueFlags::empty(), symbol_id, obj.file_id);
        } else {
            symbols_out.set_next(ValueFlags::empty(), SymbolId::undefined(), obj.file_id);
        }
    }
}

pub(super) fn populate_symbol_db<'data>(
    buckets: &mut [SymbolBucket<'data>],
    per_group_outputs: &[SymbolLoadOutputs<'data>],
) {
    timing_phase!("Populate symbol map");

    buckets.par_iter_mut().enumerate().for_each(|(b, bucket)| {
        verbose_timing_phase!("Process symbol bucket");

        // The following approximation should be an upper bound on the number of global
        // names we'll have. There will likely be at least a few global symbols with the
        // same name, in which case the actual number will be slightly smaller.
        let approx_num_symbols = per_group_outputs
            .iter()
            .map(|s| s.pending_symbols_by_bucket[b].symbols.len())
            .sum();
        bucket.name_to_id.reserve(approx_num_symbols);

        for outputs in per_group_outputs {
            let pending = &outputs.pending_symbols_by_bucket[b];

            for symbol in &pending.symbols {
                bucket.add_symbol(symbol);
            }

            for symbol in &pending.versioned_symbols {
                bucket.add_versioned_symbol(symbol);
            }
        }
    });
}

fn load_linker_script_symbols<'data, P: EnginePlatform>(
    script: &SequencedLinkerScript<'data, P>,
    symbols_out: &mut SymbolWriterShard<'_, '_, 'data, P>,
    outputs: &mut SymbolLoadOutputs<'data>,
) {
    for (offset, definition) in script.parsed.symbol_defs.iter().enumerate() {
        let symbol_id = script.symbol_id_range.offset_to_id(offset);

        if !definition.name.is_empty() {
            outputs.add_non_versioned(PendingSymbol::from_prehashed(
                symbol_id,
                PreHashed::new(
                    UnversionedSymbolName::new(definition.name),
                    hash_bytes(definition.name),
                ),
            ));
        }

        let mut flags = ValueFlags::NON_INTERPOSABLE;
        // PROVIDE_HIDDEN symbols have hidden visibility, which means they should be
        // non-interposable (already set) and not exported to dynamic symbol table.
        if definition.symbol.is_hidden() {
            flags |= ValueFlags::DOWNGRADE_TO_LOCAL;
        }
        symbols_out.set_next(flags, symbol_id, script.file_id);
    }
}

fn load_symbols_from_file<'data, P: EnginePlatform>(
    s: &SequencedInputObject<'data, P>,
    version_script: &VersionScript,
    symbols_out: &mut SymbolWriterShard<'_, '_, 'data, P>,
    outputs: &mut SymbolLoadOutputs<'data>,
    args: &P::Args,
    export_list: Option<&ExportList<'data>>,
    output_kind: OutputKind,
) -> Result {
    if s.is_dynamic() {
        DynamicObjectSymbolLoader::new(&s.parsed.object)?.load_symbols(
            s.file_id,
            symbols_out,
            outputs,
        )
    } else {
        RegularObjectSymbolLoader {
            object: &s.parsed.object,
            args,
            version_script,
            archive_semantics: s.parsed.input.has_archive_semantics(),
            lib_name: s.parsed.input.lib_name(),
            export_list,
            output_kind,
        }
        .load_symbols(s.file_id, symbols_out, outputs)
    }
}

pub(super) struct SymbolWriterShard<'out, 'group, 'data, P: Platform> {
    group: &'group Group<'data, P>,
    resolutions: sharded_vec_writer::Shard<'out, SymbolId>,
    flags: sharded_vec_writer::Shard<'out, RawFlags>,
    file_ids: sharded_vec_writer::Shard<'out, FileId>,
    next: SymbolId,
}

impl<'out, 'group, 'data, P: EnginePlatform> SymbolWriterShard<'out, 'group, 'data, P> {
    fn set_next(&mut self, flags: ValueFlags, resolution: SymbolId, file_id: FileId) {
        self.flags.push(flags.raw());
        self.resolutions.push(resolution);
        self.file_ids.push(file_id);
        self.next = SymbolId::from_usize(self.next.as_usize() + 1);
    }
}

trait SymbolLoader<'data, P: EnginePlatform> {
    fn load_symbols(
        &self,
        file_id: FileId,
        symbols_out: &mut SymbolWriterShard<'_, '_, 'data, P>,
        outputs: &mut SymbolLoadOutputs<'data>,
    ) -> Result {
        let base_symbol_id = symbols_out.next;

        for symbol in self.object().symbols_iter() {
            let symbol_id = symbols_out.next;
            let mut flags = self.compute_value_flags(symbol);
            let local_index = symbol_id.offset_from(base_symbol_id);

            if symbol.is_undefined() || self.should_ignore_symbol(symbol) {
                symbols_out.set_next(flags, SymbolId::undefined(), file_id);
                continue;
            }

            let resolution = symbol_id;

            if symbol.is_local() {
                symbols_out.set_next(flags, resolution, file_id);
                continue;
            }

            let info = self.get_symbol_name_and_version(symbol, local_index)?;

            let name = UnversionedSymbolName::prehashed(info.name());

            if self.should_downgrade_to_local(&name) {
                flags |= ValueFlags::DOWNGRADE_TO_LOCAL;
                // If we're downgrading to a local, then we're writing a shared object. Shared
                // objects should never bypass the GOT for TLS variables. However, if we're
                // downgrading all symbols by default, that'd add the flag to all symbols, so we
                // have to do this later.
                if !self.downgrades_all() && !symbol.is_tls() {
                    flags |= ValueFlags::NON_INTERPOSABLE;
                }
            }

            if info.is_default() {
                let pending = PendingSymbol::from_prehashed(symbol_id, name);
                outputs.add_non_versioned(pending);
            }

            if let Some(version) = info.version_name() {
                let pending = PendingVersionedSymbol::from_prehashed(symbol_id, name, version);
                outputs.add_versioned(pending);
            }

            symbols_out.set_next(flags, resolution, file_id);
        }

        Ok(())
    }

    fn object(&self) -> &P::File<'data>;

    fn compute_value_flags(&self, symbol: &P::SymtabEntry) -> ValueFlags;

    /// Returns whether we should downgrade a symbol with the specified name to be a local.
    fn should_downgrade_to_local(&self, _name: &PreHashed<UnversionedSymbolName>) -> bool {
        false
    }

    /// Returns whether we will downgrade all symbols by default and later upgrade some to global.
    fn downgrades_all(&self) -> bool {
        false
    }

    /// Returns whether the supplied symbol should be ignored.
    fn should_ignore_symbol(&self, _symbol: &P::SymtabEntry) -> bool {
        false
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &P::SymtabEntry,
        local_index: usize,
    ) -> Result<P::RawSymbolName<'data>>;
}

struct RegularObjectSymbolLoader<'a, 'data, P: Platform> {
    object: &'a P::File<'data>,
    args: &'a P::Args,
    version_script: &'a VersionScript<'a>,
    archive_semantics: bool,
    lib_name: &'data [u8],
    export_list: Option<&'a ExportList<'a>>,
    output_kind: OutputKind,
}

struct DynamicObjectSymbolLoader<'a, 'data, P: Platform> {
    object: &'a P::File<'data>,
    version_names: P::VersionNames<'data>,
}

impl<'a, 'data, P: EnginePlatform> DynamicObjectSymbolLoader<'a, 'data, P> {
    fn new(object: &'a P::File<'data>) -> Result<Self> {
        let version_names = object.get_version_names()?;
        Ok(Self {
            object,
            version_names,
        })
    }
}

impl<'data, P: EnginePlatform> SymbolLoader<'data, P> for RegularObjectSymbolLoader<'_, 'data, P> {
    fn compute_value_flags(&self, sym: &P::SymtabEntry) -> ValueFlags {
        let is_undefined = sym.is_undefined();

        let non_interposable = P::is_symbol_non_interposable(
            self.object,
            self.args,
            sym,
            self.output_kind,
            self.export_list,
            self.lib_name,
            self.archive_semantics,
            is_undefined,
        );

        let mut flags: ValueFlags = if sym.is_absolute() {
            ValueFlags::ABSOLUTE
        } else if sym.is_ifunc() {
            ValueFlags::IFUNC
        } else if is_undefined {
            // For undefined symbols, we tweak some of the flags later on in
            // `canonicalise_undefined_symbols`. We can't make those decisions now because we don't
            // know whether the symbol will remain undefined.
            ValueFlags::ABSOLUTE
        } else {
            ValueFlags::empty()
        };

        if non_interposable {
            flags |= ValueFlags::NON_INTERPOSABLE;
        }
        flags
    }

    fn should_downgrade_to_local(&self, name: &PreHashed<UnversionedSymbolName>) -> bool {
        match self.version_script {
            // We first downgrade all symbols when using a Rust version script.
            // We're gonna set the ones that are exported back to global later.
            VersionScript::Rust(_) => true,
            VersionScript::Regular(version_script) => version_script.is_local(name),
        }
    }

    fn downgrades_all(&self) -> bool {
        matches!(self.version_script, VersionScript::Rust(_))
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &P::SymtabEntry,
        _local_index: usize,
    ) -> Result<P::RawSymbolName<'data>> {
        Ok(<P::RawSymbolName<'data> as platform::RawSymbolName>::parse(
            self.object.symbol_name(symbol)?,
        ))
    }

    fn object(&self) -> &P::File<'data> {
        self.object
    }
}

impl<'data, P: EnginePlatform> SymbolLoader<'data, P> for DynamicObjectSymbolLoader<'_, 'data, P> {
    fn compute_value_flags(&self, symbol: &P::SymtabEntry) -> ValueFlags {
        let mut flags = ValueFlags::DYNAMIC;
        if symbol.is_func() || symbol.is_ifunc() {
            flags |= ValueFlags::DYNAMIC_FUNCTION;
        }
        if symbol.is_undefined() {
            flags |= ValueFlags::ABSOLUTE;
        }
        flags
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &P::SymtabEntry,
        local_index: usize,
    ) -> Result<P::RawSymbolName<'data>> {
        self.object
            .get_symbol_name_and_version(symbol, local_index, &self.version_names)
    }

    fn object(&self) -> &P::File<'data> {
        self.object
    }

    fn should_ignore_symbol(&self, symbol: &P::SymtabEntry) -> bool {
        // Shared objects shouldn't export hidden symbols. If for some reason they do, ignore them.
        symbol.is_hidden()
    }
}
impl<'data, P: EnginePlatform> Prelude<'data, P> {
    fn load_symbols(
        &self,
        symbols_out: &mut SymbolWriterShard<'_, '_, 'data, P>,
        outputs: &mut SymbolLoadOutputs<'data>,
    ) {
        for definition in &self.symbol_definitions {
            let symbol_id = symbols_out.next;
            let mut flags = match &definition.placement {
                SymbolPlacement::Undefined | SymbolPlacement::ForceUndefined => {
                    ValueFlags::ABSOLUTE
                }
                SymbolPlacement::SectionStart(_)
                | SymbolPlacement::SectionEnd(_)
                | SymbolPlacement::SectionGroupEnd(_)
                | SymbolPlacement::LoadBaseAddress => {
                    outputs.add_non_versioned(PendingSymbol::new(symbol_id, definition.name));
                    ValueFlags::NON_INTERPOSABLE
                }
                SymbolPlacement::PlatformSpecific(_) => {
                    outputs.add_non_versioned(PendingSymbol::new(symbol_id, definition.name));
                    ValueFlags::NON_INTERPOSABLE | ValueFlags::ABSOLUTE
                }
                SymbolPlacement::Redirect(redirect) => {
                    outputs.add_non_versioned(PendingSymbol::new(symbol_id, definition.name));
                    if matches!(redirect.loc, SymbolLoc::None) {
                        ValueFlags::NON_INTERPOSABLE | ValueFlags::ABSOLUTE
                    } else {
                        ValueFlags::NON_INTERPOSABLE
                    }
                }
            };
            if definition.symbol.is_hidden() {
                flags |= ValueFlags::DOWNGRADE_TO_LOCAL;
            }
            symbols_out.set_next(flags, symbol_id, PRELUDE_FILE_ID);
        }
    }
}
impl<P: EnginePlatform> InternalSymDefInfo<'_, P> {
    pub(crate) fn section_id(&self) -> Option<OutputSectionId> {
        match self.placement {
            SymbolPlacement::Redirect(Redirect { ref loc, .. }) => loc.section_id(),
            SymbolPlacement::Undefined
            | SymbolPlacement::ForceUndefined
            | SymbolPlacement::PlatformSpecific(_) => None,
            SymbolPlacement::SectionStart(i) => Some(i),
            SymbolPlacement::SectionEnd(i) => Some(i),
            SymbolPlacement::SectionGroupEnd(i) => Some(i),
            // The other linkers attach to the closest section, but the address is nonetheless
            // outside of the selected section. It's tricky for us to find the closest section
            // at this point in the code, so we pick an arbitrary section.
            SymbolPlacement::LoadBaseAddress => P::TEXT_SECTION_ID,
        }
    }
}
/// Decides how many buckets we should use for symbol names.
pub(super) fn num_symbol_hash_buckets(args: &impl platform::Args) -> usize {
    args.available_threads().get()
}

impl<'data> SymbolLoadOutputs<'data> {
    fn add_non_versioned(&mut self, pending: PendingSymbol<'data>) {
        let num_buckets = self.pending_symbols_by_bucket.len();

        self.pending_symbols_by_bucket[pending.name.hash() as usize % num_buckets]
            .symbols
            .push(pending);
    }

    fn add_versioned(&mut self, pending: PendingVersionedSymbol<'data>) {
        let num_buckets = self.pending_symbols_by_bucket.len();

        self.pending_symbols_by_bucket[pending.name.hash() as usize % num_buckets]
            .versioned_symbols
            .push(pending);
    }
}
