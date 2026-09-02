//! This module resolves symbol references between objects. In the process, it decides which archive
//! entries are needed. We also resolve which output section, if any, each input section should be
//! assigned to.

use crate::LayoutRules;
use crate::bail;
use crate::error::Result;
use crate::grouping::Group;
use crate::input_data::PRELUDE_FILE_ID;
use crate::output_section_id::OutputSections;
use crate::platform::Platform;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::PerSymbolFlags;
use crate::verbose_timing_phase;
use atomic_take::AtomicTake;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;

pub(crate) mod sections;
pub(crate) mod symbols;
pub(crate) mod types;

use sections::assign_section_ids;
use sections::resolve_sections;
#[allow(unused_imports)]
pub(crate) use sections::*;
use symbols::canonicalise_undefined_symbols;
use symbols::process_object;
use symbols::work_items_do;
#[allow(unused_imports)]
pub(crate) use symbols::*;
use types::LoadObjectSymbolsRequest;
use types::Outputs;
use types::UndefinedSymbol;
#[allow(unused_imports)]
pub(crate) use types::*;

pub(crate) struct Resolver<'data, P: Platform> {
    undefined_symbols: Vec<UndefinedSymbol<'data>>,
    pub(crate) resolved_groups: Vec<ResolvedGroup<'data, P>>,
}

impl<'data, P: Platform> Resolver<'data, P> {
    /// Resolves undefined symbols. In the process of resolving symbols, we decide which archive
    /// entries to load. Some symbols may not have definitions, in which case we'll note those for
    /// later processing. Can be called multiple times with additional groups having been added to
    /// the SymbolDb in between.
    pub(crate) fn resolve_symbols_and_select_archive_entries(
        &mut self,
        symbol_db: &mut SymbolDb<'data, P>,
        per_symbol_flags: &mut PerSymbolFlags,
    ) -> Result {
        resolve_symbols_and_select_archive_entries(self, symbol_db, per_symbol_flags)
    }

    /// For all regular objects that we've decided to load, decide what to do with each section.
    /// Canonicalises undefined symbols. Some undefined symbols might be able to become defined if
    /// we can identify them as start/stop symbols for which we found a custom section with the
    /// appropriate name.
    pub(crate) fn resolve_sections_and_canonicalise_undefined(
        mut self,
        symbol_db: &mut SymbolDb<'data, P>,
        per_symbol_flags: &mut PerSymbolFlags,
        output_sections: &mut OutputSections<'data, P>,
        layout_rules: &LayoutRules<'data>,
    ) -> Result<Vec<ResolvedGroup<'data, P>>> {
        timing_phase!("Section resolution");

        resolve_sections(
            &mut self.resolved_groups,
            symbol_db,
            layout_rules,
            output_sections,
        )?;

        let mut syn = symbol_db.new_synthetic_symbols_group();

        assign_section_ids(
            &mut self.resolved_groups,
            &mut symbol_db.section_part_ids,
            output_sections,
            symbol_db.args,
        );

        // Apply -Ttext/-Tdata/-Tbss (and --section-start) overrides to built-in sections.
        output_sections.apply_section_start_overrides(symbol_db.args);

        canonicalise_undefined_symbols(
            self.undefined_symbols,
            output_sections,
            &self.resolved_groups,
            symbol_db,
            per_symbol_flags,
            &mut syn,
        );

        self.resolved_groups.push(ResolvedGroup {
            files: vec![ResolvedFile::SyntheticSymbols(syn)],
        });

        Ok(self.resolved_groups)
    }
}

fn resolve_symbols_and_select_archive_entries<'data, P: Platform>(
    resolver: &mut Resolver<'data, P>,
    symbol_db: &mut SymbolDb<'data, P>,
    per_symbol_flags: &mut PerSymbolFlags,
) -> Result {
    timing_phase!("Resolve symbols");

    // Note, this is the total number of objects including those that we might have processed in
    // previous calls. This is just an upper bound on how many objects might need to be loaded. We
    // can't just count the objects in the new groups because we might end up loading some of the
    // objects from earlier groups.
    let num_regular_objects = symbol_db.num_regular_objects();
    let num_lto_objects = symbol_db.num_lto_objects();
    if num_regular_objects == 0 && num_lto_objects == 0 {
        bail!("no input files");
    }

    let mut symbol_definitions = symbol_db.take_definitions();
    let mut symbol_definitions_slice: &mut [SymbolId] = symbol_definitions.as_mut();

    let mut definitions_per_group_and_file = Vec::new();
    definitions_per_group_and_file.resize_with(symbol_db.groups.len(), Vec::new);

    let outputs = {
        verbose_timing_phase!("Allocate outputs store");
        Outputs::new(num_regular_objects, num_lto_objects)
    };

    let mut initial_work = Vec::new();

    {
        verbose_timing_phase!("Resolution setup");

        let pre_existing_groups = resolver.resolved_groups.len();
        let new_groups = &symbol_db.groups[pre_existing_groups..];

        for (group, definitions_out_per_file) in resolver
            .resolved_groups
            .iter()
            .zip(&mut definitions_per_group_and_file)
        {
            *definitions_out_per_file = group
                .files
                .iter()
                .map(|file| {
                    let definitions = symbol_definitions_slice
                        .split_off_mut(..file.symbol_id_range().len())
                        .unwrap();

                    if matches!(file, ResolvedFile::NotLoaded(_)) {
                        AtomicTake::new(definitions)
                    } else {
                        AtomicTake::empty()
                    }
                })
                .collect();
        }

        resolver.resolved_groups.extend(
            new_groups
                .iter()
                .zip(&mut definitions_per_group_and_file[pre_existing_groups..])
                .map(|(group, definitions_out_per_file)| {
                    resolve_group(
                        group,
                        &mut initial_work,
                        definitions_out_per_file,
                        &mut symbol_definitions_slice,
                        symbol_db,
                        &outputs,
                    )
                }),
        );
    };

    let atomic_per_symbol_flags = per_symbol_flags.borrow_atomic();

    let resources = ResolutionResources {
        definitions_per_file: &definitions_per_group_and_file,
        symbol_db,
        outputs: &outputs,
        per_symbol_flags: &atomic_per_symbol_flags,
    };

    rayon::in_place_scope(|scope| {
        initial_work.into_par_iter().for_each(|work_item| {
            process_object(work_item, &resources, scope);
        });
    });

    {
        verbose_timing_phase!("Drop definitions_per_group_and_file");
        drop(definitions_per_group_and_file);
    }

    symbol_db.restore_definitions(symbol_definitions);

    if let Some(e) = outputs.errors.pop() {
        return Err(e);
    }

    verbose_timing_phase!("Gather loaded objects");

    for obj in outputs.loaded {
        let file_id = match &obj {
            ResolvedFile::Object(o) => o.common.file_id,
            ResolvedFile::Dynamic(o) => o.common.file_id,
            _ => unreachable!(),
        };
        resolver.resolved_groups[file_id.group()].files[file_id.file()] = obj;
    }

    #[cfg(all(feature = "plugins", unix))]
    for obj in outputs.loaded_lto_objects {
        let file_id = obj.file_id;
        resolver.resolved_groups[file_id.group()].files[file_id.file()] =
            ResolvedFile::LtoInput(obj);
    }

    resolver.undefined_symbols.extend(outputs.undefined_symbols);

    Ok(())
}

fn resolve_group<'data, 'definitions, P: Platform>(
    group: &Group<'data, P>,
    initial_work_out: &mut Vec<LoadObjectSymbolsRequest<'definitions>>,
    definitions_out_per_file: &mut Vec<AtomicTake<&'definitions mut [SymbolId]>>,
    symbol_definitions_slice: &mut &'definitions mut [SymbolId],
    symbol_db: &SymbolDb<'data, P>,
    outputs: &Outputs<'data, P>,
) -> ResolvedGroup<'data, P> {
    let start_defs_len = symbol_definitions_slice.len();

    let resolved_group = match group {
        Group::Prelude(prelude) => {
            let definitions_out = symbol_definitions_slice
                .split_off_mut(..prelude.symbol_definitions.len())
                .unwrap();

            work_items_do(
                PRELUDE_FILE_ID,
                definitions_out,
                symbol_db,
                outputs,
                |work_item| {
                    initial_work_out.push(work_item);
                },
            );

            definitions_out_per_file.push(AtomicTake::empty());

            ResolvedGroup {
                files: vec![ResolvedFile::Prelude(ResolvedPrelude {
                    symbol_definitions: prelude.symbol_definitions.clone(),
                })],
            }
        }
        Group::Objects(parsed_input_objects) => {
            definitions_out_per_file.reserve(parsed_input_objects.len());

            let files = parsed_input_objects
                .iter()
                .map(|s| {
                    let definitions_out = symbol_definitions_slice
                        .split_off_mut(..s.symbol_id_range.len())
                        .unwrap();

                    if s.is_optional() {
                        definitions_out_per_file.push(AtomicTake::new(definitions_out));
                    } else {
                        work_items_do(
                            s.file_id,
                            definitions_out,
                            symbol_db,
                            outputs,
                            |work_item| {
                                initial_work_out.push(work_item);
                            },
                        );
                        definitions_out_per_file.push(AtomicTake::empty());
                    }

                    ResolvedFile::NotLoaded(NotLoaded {
                        symbol_id_range: s.symbol_id_range,
                        section_id_range: s.section_id_range,
                    })
                })
                .collect();

            ResolvedGroup { files }
        }
        Group::StubLibraries(stubs) => {
            let files = stubs
                .iter()
                .map(|stub| {
                    symbol_definitions_slice
                        .split_off_mut(..stub.symbol_id_range.len())
                        .unwrap();
                    definitions_out_per_file.push(AtomicTake::empty());
                    ResolvedFile::StubLibrary(ResolvedStubLibrary {
                        input: stub.input,
                        file_id: stub.file_id,
                        symbol_id_range: stub.symbol_id_range,
                        // TODO: Consider alternative to cloning this.
                        defined_symbols: stub.defined_symbols.clone(),
                    })
                })
                .collect();

            ResolvedGroup { files }
        }
        Group::LinkerScripts(scripts) => {
            let files = scripts
                .iter()
                .map(|s| {
                    let definitions_out = symbol_definitions_slice
                        .split_off_mut(..s.symbol_id_range.len())
                        .unwrap();

                    definitions_out_per_file.push(AtomicTake::empty());

                    initial_work_out.push(LoadObjectSymbolsRequest {
                        file_id: s.file_id,
                        symbol_start_offset: 0,
                        definitions_out,
                    });

                    ResolvedFile::LinkerScript(ResolvedLinkerScript {
                        input: s.parsed.input,
                        file_id: s.file_id,
                        symbol_id_range: s.symbol_id_range,
                        // TODO: Consider alternative to cloning this.
                        symbol_definitions: s.parsed.symbol_defs.clone(),
                    })
                })
                .collect();

            ResolvedGroup { files }
        }
        Group::SyntheticSymbols(syn) => {
            symbol_definitions_slice
                .split_off_mut(..syn.symbol_id_range.len())
                .unwrap();

            definitions_out_per_file.push(AtomicTake::empty());

            ResolvedGroup {
                files: vec![ResolvedFile::SyntheticSymbols(ResolvedSyntheticSymbols {
                    file_id: syn.file_id,
                    start_symbol_id: syn.symbol_id_range.start(),
                    symbol_definitions: Vec::new(),
                })],
            }
        }
        #[cfg(all(feature = "plugins", unix))]
        Group::LtoInputs(lto_objects) => ResolvedGroup {
            files: lto_objects
                .iter()
                .map(|o| {
                    let definitions_out = symbol_definitions_slice
                        .split_off_mut(..o.symbol_id_range.len())
                        .unwrap();

                    if o.is_optional() {
                        definitions_out_per_file.push(AtomicTake::new(definitions_out));
                    } else {
                        work_items_do(
                            o.file_id,
                            definitions_out,
                            symbol_db,
                            outputs,
                            |work_item| {
                                initial_work_out.push(work_item);
                            },
                        );
                        definitions_out_per_file.push(AtomicTake::empty());
                    }

                    ResolvedFile::NotLoaded(NotLoaded {
                        symbol_id_range: o.symbol_id_range,
                        section_id_range: o.section_id_range,
                    })
                })
                .collect(),
        },
    };

    // Every call to this function must consume a number of definitions equal to the group's symbol
    // count, otherwise subsequent calls will end up writing to the wrong part of the slice.
    let taken = start_defs_len - symbol_definitions_slice.len();
    assert_eq!(
        taken,
        group.num_symbols(),
        "resolve_group({group}) took incorrect number of symbol defs"
    );

    resolved_group
}
impl<'data, P: Platform> Default for Resolver<'data, P> {
    fn default() -> Self {
        Self {
            undefined_symbols: Default::default(),
            resolved_groups: Default::default(),
        }
    }
}
