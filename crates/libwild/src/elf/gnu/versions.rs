use crate::args::elf::ElfArgs;
#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::output::*;
use crate::elf::part_id;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::DynamicSymbolDefinition;
use crate::output_kind::OutputKind;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform;
use crate::platform::Platform;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use hashbrown::HashMap;
use object::LittleEndian;
use rayon::prelude::*;

pub(crate) struct VersionNames<'data> {
    pub(crate) names: Vec<Option<&'data [u8]>>,
}

#[derive(Debug)]
pub(crate) struct RawSymbolName<'data> {
    pub(crate) name: &'data [u8],

    pub(crate) version_name: Option<&'data [u8]>,

    /// Whether the symbol can be referred to without a version.
    pub(crate) is_default: bool,
}

impl<'data> platform::RawSymbolName<'data> for RawSymbolName<'data> {
    fn parse(mut name_bytes: &'data [u8]) -> Self {
        let mut version_name = None;
        let mut is_default = true;

        // Symbols can contain version specifiers, e.g. `foo@1.1` or `foo@@2.0`. The latter,
        // with double-at specifies that it's the default version.
        if let Some(at_offset) = memchr::memchr(b'@', name_bytes) {
            if name_bytes[at_offset..].starts_with(b"@@") {
                version_name = Some(&name_bytes[at_offset + 2..]);
            } else {
                version_name = Some(&name_bytes[at_offset + 1..]);
                is_default = false;
            }

            name_bytes = &name_bytes[..at_offset];
        }

        RawSymbolName {
            name: name_bytes,
            version_name,
            is_default,
        }
    }

    fn name(&self) -> &'data [u8] {
        self.name
    }

    fn version_name(&self) -> Option<&'data [u8]> {
        self.version_name
    }

    fn is_default(&self) -> bool {
        self.is_default
    }
}

impl std::fmt::Display for RawSymbolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.name))?;
        if let Some(version) = self.version_name {
            if self.is_default {
                write!(f, "@@")?;
            } else {
                write!(f, "@")?;
            }
            write!(f, "{}", String::from_utf8_lossy(version))?;
        }

        Ok(())
    }
}

pub(crate) struct VerneedTable<'data> {
    pub(crate) versym: &'data [Versym],
    pub(crate) version_names_by_index: Vec<Option<&'data [u8]>>,
}

impl<'data> VerneedTable<'data> {
    pub(crate) fn new<C: ElfClass>(file: &File<'data, C>) -> Result<Self> {
        Ok(Self {
            versym: file.versym,
            version_names_by_index: verneed_names_by_index(file)?,
        })
    }
}

impl<'data> platform::VerneedTable<'data> for VerneedTable<'data> {
    fn version_name(&self, local_symbol_index: object::SymbolIndex) -> Option<&'data [u8]> {
        let version_index = self.versym.get(local_symbol_index.0)?.0.get(LittleEndian);
        self.version_names_by_index
            .get(usize::from(version_index.index()))
            .copied()
            .flatten()
    }
}

pub(crate) fn verneed_names_by_index<'data, C: ElfClass>(
    file: &File<'data, C>,
) -> Result<Vec<Option<&'data [u8]>>> {
    let mut version_names = Vec::new();
    let endian = LittleEndian;

    if let Some((verneeds, string_table_index)) = &file.verneed {
        let strings = file
            .sections
            .strings(endian, file.data, *string_table_index)?;

        for r in verneeds.clone() {
            let (_verneed, aux_iterator) = r?;
            for aux in aux_iterator {
                let aux = aux?;
                let version_index = usize::from(aux.vna_other.get(endian));
                let name = aux.name(endian, strings)?;

                if version_names.len() <= version_index {
                    version_names.resize_with(version_index + 1, || None);
                }
                version_names[version_index] = Some(name);
            }
        }
    }

    Ok(version_names)
}

#[derive(Debug)]
pub(crate) struct VerneedInfo<'data, C: ElfClass> {
    pub(crate) defs: VerdefIterator<'data, C>,
    pub(crate) string_table_index: object::SectionIndex,

    /// Number of symbol versions that we're going to emit. This is the number of entries in
    /// `symbol_versions_needed` that are true. Computed after graph traversal.
    pub(crate) version_count: u16,
}

#[derive(Default)]
pub(crate) struct DynamicLayoutStateExt<'data, C: ElfClass> {
    /// Which symbol versions are needed. A symbol version is needed if a symbol with that version
    /// has been loaded. The first version has index 1, so we store it at offset 0.
    pub(crate) symbol_versions_needed: Vec<bool>,

    pub(crate) verneed_info: Option<VerneedInfo<'data, C>>,

    pub(crate) non_addressable_indexes: NonAddressableIndexes,

    /// Maps from addresses within the shared object to copy relocations at that address.
    pub(crate) copy_relocations: HashMap<u64, CopyRelocationInfo>,
}

#[derive(Debug)]
pub(crate) struct DynamicLayoutExt<'data, C: ElfClass> {
    /// Mapping from input versions to output versions. Input version 1 is at index 0.
    pub(crate) version_mapping: Vec<object::elf::VersionIndex>,

    pub(crate) verneed_info: Option<VerneedInfo<'data, C>>,

    /// Whether this is the last DynamicLayout that puts content into .gnu.version_r.
    pub(crate) is_last_verneed: bool,

    pub(crate) copy_relocation_symbols: Vec<SymbolId>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct NonAddressableIndexes {
    /// The version index that will be used for the next `.gnu.version_r` entry that we define.
    pub(crate) next_gnu_version_r_index: object::elf::VersionIndex,
}

impl platform::NonAddressableIndexes for NonAddressableIndexes {
    fn new<P: Platform>(symbol_db: &P::SymbolDb<'_>) -> Self {
        Self {
            // Allocate version indexes starting from after the local and global indexes and any
            // versions defined by a version script.
            next_gnu_version_r_index: object::elf::VER_NDX_GLOBAL
                + 1.max(P::version_script_version_count(symbol_db)),
        }
    }
}

pub(crate) struct CopyRelocationInfo {
    /// The symbol ID for which we'll actually generate the copy relocation. Initially, this is
    /// just the first symbol at a particular address for which we requested a copy relocation,
    /// then later we may update it to point to a different symbol if that first symbol was
    /// weak.
    pub(crate) symbol_id: SymbolId,

    pub(crate) is_weak: bool,
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct NonAddressableCounts {
    /// The number of shared objects that want to emit a verneed record.
    pub(crate) verneed_count: u64,
    /// The number of verdef records provided in version script.
    pub(crate) verdef_count: u16,
}

#[derive(Debug)]
pub(crate) struct EpilogueLayoutExt {
    pub(crate) sysv_hash_layout: Option<SysvHashLayout>,
    pub(crate) gnu_hash_layout: Option<GnuHashLayout>,
    pub(crate) verdefs: Option<Vec<VersionDef>>,
    pub(crate) build_id_size: Option<usize>,
    pub(crate) needs_eh_frame_terminator: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GnuHashLayout {
    pub(crate) num_defs: u32,
    pub(crate) bucket_count: u32,
    pub(crate) bloom_shift: u32,
    pub(crate) bloom_count: u32,
    pub(crate) symbol_base: u32,
}

pub(crate) fn create_gnu_hash_layout<C: ElfClass>(
    args: &ElfArgs,
    output_kind: OutputKind,
    dynamic_symbol_definitions: &mut [DynamicSymbolDefinition<'_, Elf<C>>],
) -> Option<GnuHashLayout> {
    if !args.hash_style.includes_gnu() || !output_kind.needs_dynamic() {
        return None;
    }

    // Our number of buckets is computed somewhat arbitrarily so that we have on average 2
    // symbols per bucket, but then we round up to a power of two.
    let num_defs = dynamic_symbol_definitions.len();
    let gnu_hash_layout = GnuHashLayout {
        num_defs: dynamic_symbol_definitions.len() as u32,
        bucket_count: (num_defs / 2).next_power_of_two() as u32,
        bloom_shift: 6,
        bloom_count: 1,
        // `symbol_base` is set later in `finalise_layout`.
        symbol_base: 0,
    };

    // If we're going to emit .gnu.hash, then we need to stort the dynamic symbols by bucket.
    // Tie-break by name for determinism. We can use an unstable sort because names should be
    // unique. We use a parallel sort because we're processing symbols from potentially many
    // input objects, so there can be a lot.
    dynamic_symbol_definitions.par_sort_unstable_by_key(|d| {
        (
            gnu_hash_layout.bucket_for_hash(d.format_specific.hash),
            d.name,
        )
    });

    Some(gnu_hash_layout)
}

impl GnuHashLayout {
    /// Allocates space required for .gnu.hash. Also sorts dynamic symbol definitions by their hash
    /// bucket as required by .gnu.hash.
    pub(crate) fn allocate<C: ElfClass>(&self, mem_sizes: &mut OutputSectionPartMap<u64>) {
        mem_sizes.increment(
            part_id::GNU_HASH,
            (size_of::<GnuHashHeader>()
                + C::GNU_HASH_BLOOM_SIZE as usize * self.bloom_count as usize
                + size_of::<u32>() * self.bucket_count as usize
                + size_of::<u32>() * self.num_defs as usize) as u64,
        );
    }

    pub(crate) fn bucket_for_hash(&self, hash: u32) -> u32 {
        hash % self.bucket_count
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SysvHashLayout {
    pub(crate) bucket_count: u32,
    pub(crate) chain_count: u32,
}

#[derive(derive_more::Debug)]
pub(crate) struct VersionDef {
    #[debug("{}", String::from_utf8_lossy(name))]
    pub(crate) name: Vec<u8>,
    pub(crate) parent_index: Option<u16>,
}

impl SysvHashLayout {
    pub(crate) fn byte_size(self) -> Result<u64> {
        let words = 2u64
            .checked_add(u64::from(self.bucket_count))
            .and_then(|v| v.checked_add(u64::from(self.chain_count)))
            .context("Too many dynamic symbols for .hash")?;
        Ok(words * size_of::<u32>() as u64)
    }
}

pub(crate) fn finalise_gnu_version_size<'data, C: ElfClass>(
    mem_sizes: &mut OutputSectionPartMap<u64>,
    symbol_db: &SymbolDb<'data, crate::elf::Elf<C>>,
) {
    if symbol_db.output_kind.should_output_symbol_versions() {
        let num_dynamic_symbols = mem_sizes.get(part_id::DYNSYM) / C::SYMTAB_ENTRY_SIZE;
        // Note, sets the GNU_VERSION allocation rather than incrementing it. Assuming there are
        // multiple files in our group, we'll update this same value multiple times, each time
        // with a possibly revised dynamic symbol count. The important thing is that when we're
        // done finalising the group sizes, the GNU_VERSION size should be consistent with the
        // DYNSYM size.
        *mem_sizes.get_mut(part_id::GNU_VERSION) =
            num_dynamic_symbols * crate::elf::GNU_VERSION_ENTRY_SIZE;
    }
}
