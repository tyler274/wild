//! Input sections that are marked as string-merge sections need special processing. Our algorithm
//! is somewhat complicated in an attempt to get good performance. A rough outline of our algorithm
//! is here with more details throughout the code. Contrary to what the name might suggest, this
//! algorithm also supports merging non-string sections. String sections are split at null
//! terminators. Non-string sections with `sh_entsize > 1` are split into that many bytes;
//! otherwise the whole section is treated as a single slice.
//!
//! When an output section contains both strings and constant-pool units (entsize > 1),
//! or strings of different alignments, they are hashed into separate bucket ranges so that
//! high alignment does not pad lower-alignment strings. Constant pools of different
//! `(entsize, alignment)` are also kept apart so a `.rodata.cst2` unit cannot pad a
//! 64-byte crypto table out to the next 64-byte boundary.
//!
//! We group input sections by the output section into which they are to be placed. We then process
//! each output section one at a time.
//!
//! Taking all the input sections for a particular output section, we group adjacent input sections
//! so that each group has a roughly similar size in bytes.
//!
//! With multiple threads, we alternate between two phases:
//!
//! Phase 1: We take the whole input sections or split string sections by looking for null
//! terminators, then we hash the resulting slices and store it in a bucket based on its hash.
//!
//! Phase 2: We take the outputs of phase 1 and insert the slices into a hashmap for the bucket
//! that the slice is in. As we do this, we compute bucket-relative offsets for each string and
//! store these into entries in a map that we set up in phase 1.
//!
//! Threads can switch between phases multiple times until all work for the section is complete. At
//! that point, we do some finishing work single-threaded such as computing the starting offset of
//! each bucket and populating a hashmap from input to output offset for any offsets that didn't fit
//! in our primary offset map.

use crate::alignment;
use crate::args::Experiment;
use crate::bail;
use crate::error::Context as _;
use crate::error::Result;
use crate::hash::PassThroughHashMap;
use crate::hash::PreHashed;
use crate::input_section_id::SectionIdRange;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Symbol as _;
use crate::resolution::ResolvedFile;
use crate::resolution::ResolvedGroup;
use crate::resolution::SectionSlot;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crossbeam_queue::ArrayQueue;
use crossbeam_utils::atomic::AtomicCell;
use hashbrown::HashMap;
use itertools::Itertools as _;
use rayon::Scope;
use sharded_offset_map::OffsetMap;
use sharded_offset_map::ShardedWriter;
use std::cell::RefCell;
use std::mem::replace;
use std::mem::take;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use thread_local::ThreadLocal;

/// Maximum number of threads that can split and hash input sections at once. We default to allowing
/// splitting parallelism up to the number of threads, but beyond about 24 it doesn't really help.
const MAX_SPLIT_PARALLELISM: u64 = 24;

/// How large should our chunks of input bytes be.
const TARGET_GROUP_SIZE_BYTES: u64 = 140_000;

/// Setting this to a higher value increases the potential for parallelism of hash table population
/// and gives better cache performance. However, it also increases heap allocations. Changing this
/// value will result in a different ordering of strings within the output file.
const MERGE_STRING_BUCKET_BITS: usize = 4;
const MERGE_STRING_BUCKETS: usize = 1 << MERGE_STRING_BUCKET_BITS;

/// Number of input offsets to represent by a single block. A block can store up to 12 offsets. If
/// we get more than 12 offsets within a block, then we need to spill the offset to a hashmap.
/// Increasing this value decreases memory usage, however it may result in more offsets being
/// spilled to the hashmap.
const MAP_BLOCK_SIZE: u64 = 256;

pub(crate) struct StringMergeInputs<'data> {
    input_sections_by_output: OutputSectionMap<Vec<StringMergeInputSection<'data>>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StringMergeSectionSlot {
    /// The sum of the sizes of the input sections prior to this one with the same part ID.
    /// Populated during string merging.
    start_input_offset: LinearInputOffset,
}

impl StringMergeSectionSlot {
    pub(crate) fn new() -> Self {
        Self {
            // We'll fill this in during string merging.
            start_input_offset: LinearInputOffset(0),
        }
    }
}

/// Extra stuff that we don't want to put in `StringMergeSectionSlot` because like all section
/// slots, we want to keep it as small as possible.
#[derive(Debug)]
pub(crate) struct StringMergeSectionExtra<'data> {
    pub(crate) index: object::SectionIndex,
    pub(crate) section_data: &'data [u8],
    pub(crate) is_strings: bool,
    pub(crate) alignment: alignment::Alignment,
    pub(crate) entsize: u64,
}

/// An input offset. We pretend that we've placed all input sections for a given output section one
/// after the other. This offset is then the offset into that space.
#[derive(Debug, Copy, Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct LinearInputOffset(u64);

impl std::ops::Add<u64> for LinearInputOffset {
    type Output = LinearInputOffset;

    fn add(self, rhs: u64) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl std::ops::Sub<LinearInputOffset> for LinearInputOffset {
    type Output = u64;

    fn sub(self, rhs: LinearInputOffset) -> Self::Output {
        self.0 - rhs.0
    }
}

#[derive(Clone, Copy)]
struct StringMergeInputSection<'data> {
    section_data: &'data [u8],

    /// The sum of the sizes of the input sections prior to this one with the same `part_id`.
    start_input_offset: LinearInputOffset,

    is_string: bool,

    /// `sh_addralign` of the input section. Strings from different alignments are not deduped
    /// and are placed at offsets congruent to 0 modulo this alignment.
    alignment: alignment::Alignment,

    /// `sh_entsize`. Non-string merge sections with entsize > 1 are split into that many bytes.
    entsize: u64,
}

impl StringMergeInputSection<'_> {
    /// `.rodata.cst8` / `.cst16` / … - kept in their own merge class so their
    /// alignment does not pad strings.
    fn is_constant_pool(self) -> bool {
        !self.is_string && self.entsize > 1
    }

    /// Alignment used when packing units of this section. GNU ld aligns
    /// `SHF_MERGE` constant-pool entities to `max(sh_addralign, sh_entsize)` when
    /// `sh_entsize` is a power of two.
    fn layout_alignment(self) -> alignment::Alignment {
        if self.is_constant_pool() {
            alignment::Alignment::new(self.entsize)
                .ok()
                .map(|entsize_align| self.alignment.max(entsize_align))
                .unwrap_or(self.alignment)
        } else {
            self.alignment
        }
    }

    /// GNU ld does not mix SHF_MERGE inputs of different alignment (or strings vs
    /// constants, or constant pools of different entsize). We approximate that by
    /// hashing each class into its own bucket range so intra-bucket padding cannot
    /// blow up `.rodata`.
    fn merge_class_key(self) -> u32 {
        if self.is_constant_pool() {
            // Distinct from string keys (alignment exponents are small).
            0x8000_0000 | ((self.entsize as u32) << 8) | u32::from(self.alignment.exponent)
        } else {
            ((self.entsize.max(1) as u32) << 8) | u32::from(self.alignment.exponent)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MergeClassBuckets {
    key: u32,
    base: usize,
    count: usize,
    pad_align: alignment::Alignment,
}

fn build_merge_class_buckets(
    input_sections: &[StringMergeInputSection<'_>],
) -> Vec<MergeClassBuckets> {
    let mut keys = Vec::new();
    for s in input_sections {
        let key = s.merge_class_key();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return vec![MergeClassBuckets {
            key: 0,
            base: 0,
            count: MERGE_STRING_BUCKETS,
            pad_align: alignment::MIN,
        }];
    }
    let pad_align_for = |key: u32| {
        input_sections
            .iter()
            .filter(|s| s.merge_class_key() == key)
            .map(|s| s.alignment)
            .max()
            .unwrap_or(alignment::MIN)
    };
    if keys.len() == 1 {
        return vec![MergeClassBuckets {
            key: keys[0],
            base: 0,
            count: MERGE_STRING_BUCKETS,
            pad_align: pad_align_for(keys[0]),
        }];
    }
    let n = keys.len().min(MERGE_STRING_BUCKETS);
    let mut remaining_buckets = MERGE_STRING_BUCKETS;
    let mut remaining_classes = n;
    let mut base = 0;
    keys.into_iter()
        .take(n)
        .map(|key| {
            let count = remaining_buckets / remaining_classes;
            let class = MergeClassBuckets {
                key,
                base,
                count,
                pad_align: pad_align_for(key),
            };
            remaining_buckets -= count;
            remaining_classes -= 1;
            base += count;
            class
        })
        .collect()
}

fn merge_bucket_index(
    hash: u64,
    section: &StringMergeInputSection<'_>,
    classes: &[MergeClassBuckets],
) -> usize {
    let key = section.merge_class_key();
    let class = classes
        .iter()
        .find(|c| c.key == key)
        .or_else(|| classes.last())
        .unwrap();
    class.base + (hash as usize % class.count)
}

/// Restore each class to its unpadded tail-merged size, then pad so class
/// starts are aligned in the output VMA (`start_vma` is the address of merge
/// offset 0). GNU ld aligns `SHF_MERGE` groups to absolute VMA, not to offset
/// 0 of the merged blob. Returns leading padding before the first class.
fn pad_merge_buckets(
    buckets: &mut [MergeStringsSectionBucket<'_>],
    classes: &[MergeClassBuckets],
    start_vma: u64,
    class_unpadded: &[u32],
) -> u32 {
    for (class, &sz) in classes.iter().zip(class_unpadded.iter()) {
        for bucket in buckets.iter_mut().skip(class.base).take(class.count) {
            bucket.next_offset = if bucket.index == class.base { sz } else { 0 };
        }
        let dest = &mut buckets[class.base];
        dest.next_offset = class.pad_align.align_up(u64::from(dest.next_offset)) as u32;
    }

    let mut leading_pad = 0u32;
    let mut offset = 0u64;
    for (i, class) in classes.iter().enumerate() {
        let aligned = class.pad_align.align_up(start_vma + offset);
        let extra = aligned - (start_vma + offset);
        if extra > 0 {
            if i == 0 {
                leading_pad = extra as u32;
            } else {
                let prev = &classes[i - 1];
                buckets[prev.base + prev.count - 1].next_offset += extra as u32;
            }
            offset += extra;
        }
        offset += u64::from(buckets[class.base].next_offset);
    }
    leading_pad
}

struct TailMergePiece<'data> {
    bytes: &'data [u8],
    alignment: alignment::Alignment,
    is_string: bool,
    entsize: u32,
    old_bucket: usize,
    old_offset: u32,
    first_input: LinearInputOffset,
    absorbed_by: Option<usize>,
}

/// BFD `record_section` entity alignment: the largest power of two that
/// divides `offset_in_section`, capped at the input section's `sh_addralign`.
fn entity_alignment(
    offset_in_section: u64,
    section_align: alignment::Alignment,
) -> alignment::Alignment {
    let mask = section_align.mask();
    let mut eltalign = offset_in_section;
    eltalign = ((eltalign ^ eltalign.wrapping_sub(1)).wrapping_add(1)) >> 1;
    if eltalign == 0 || eltalign > mask {
        section_align
    } else {
        alignment::Alignment::new(eltalign).unwrap_or(section_align)
    }
}

/// Reverse-compare like BFD `strrevcmp` / `strrevcmp_align`: content excluding the
/// trailing NUL, optionally grouped by `len % alignment` first.
fn gnu_strrev_cmp(
    a: &[u8],
    b: &[u8],
    align: u64,
    use_align: bool,
    entsize: usize,
) -> std::cmp::Ordering {
    let la = a.len().saturating_sub(entsize);
    let lb = b.len().saturating_sub(entsize);
    if use_align && align > 1 {
        let mask = (align as usize).wrapping_sub(1);
        let ta = la & mask;
        let tb = lb & mask;
        if ta != tb {
            return ta.cmp(&tb);
        }
    }
    let l = la.min(lb);
    for i in 0..l {
        let ca = a[la - 1 - i];
        let cb = b[lb - 1 - i];
        if ca != cb {
            return ca.cmp(&cb);
        }
    }
    la.cmp(&lb)
}

/// GNU ld tail-merges `SHF_STRINGS`: `"World"` shares storage with `"HelloWorld"` when
/// they are adjacent after reverse-sort and the suffix offset satisfies alignment
/// (bfd `merge.c` `merge_strings`). Hash buckets cannot see cross-bucket suffixes,
/// so after unique-ing we re-layout each merge class as a linear pool.
fn apply_string_tail_merge(
    buckets: &mut [MergeStringsSectionBucket<'_>],
    classes: &[MergeClassBuckets],
    tail_remap: &mut HashMap<u32, BucketOffset>,
) -> Result {
    for class in classes {
        tail_merge_class(buckets, class, tail_remap)?;
    }
    Ok(())
}

fn tail_merge_class(
    buckets: &mut [MergeStringsSectionBucket<'_>],
    class: &MergeClassBuckets,
    tail_remap: &mut HashMap<u32, BucketOffset>,
) -> Result {
    let mut pieces = Vec::new();
    for bucket in buckets.iter().skip(class.base).take(class.count) {
        for s in &bucket.strings {
            pieces.push(TailMergePiece {
                bytes: s.bytes,
                alignment: s.alignment,
                is_string: s.is_string,
                entsize: s.entsize,
                old_bucket: bucket.index,
                old_offset: s.offset,
                first_input: s.first_input,
                absorbed_by: None,
            });
        }
    }
    if pieces.is_empty() {
        return Ok(());
    }

    // BFD merge_strings: reverse-sort, then absorb a string only into the
    // immediately neighboring longer host. All-pairs suffix matching over-merges
    // relative to GNU ld (kernel .rodata was 0x100 smaller).
    let mut order: Vec<usize> = (0..pieces.len()).filter(|&i| pieces[i].is_string).collect();
    if order.len() >= 2 {
        let align = pieces[order[0]].alignment.value();
        let entsize = pieces[order[0]].entsize.max(1) as usize;
        let use_align =
            align > entsize as u64 && order.iter().all(|&i| pieces[i].alignment.value() == align);
        order.sort_by(|&i, &j| {
            gnu_strrev_cmp(pieces[i].bytes, pieces[j].bytes, align, use_align, entsize)
        });
        let mut host = order[order.len() - 1];
        for k in (0..order.len() - 1).rev() {
            let cmp = order[k];
            let host_bytes = pieces[host].bytes;
            let cmp_bytes = pieces[cmp].bytes;
            let cmp_align = pieces[cmp].alignment.value() as usize;
            if pieces[host].alignment.value() >= pieces[cmp].alignment.value()
                && host_bytes.len() > cmp_bytes.len()
                && cmp_align != 0
                && (host_bytes.len() - cmp_bytes.len()).is_multiple_of(cmp_align)
                && host_bytes.ends_with(cmp_bytes)
            {
                pieces[cmp].absorbed_by = Some(host);
            } else {
                host = cmp;
            }
        }
    }

    let dest_bucket = class.base;
    let mut new_strings = Vec::new();
    let mut next_offset = 0u32;
    let mut laid_out = vec![None; pieces.len()];
    // BFD lays out hash-insertion order (`htab->first`), which is first-seen
    // input order. `size += len` (not `align_up(len)`); the next string is
    // then aligned to its own requirement.
    let mut kept: Vec<usize> = (0..pieces.len())
        .filter(|&i| pieces[i].absorbed_by.is_none())
        .collect();
    kept.sort_by_key(|&i| pieces[i].first_input);
    let string_class = pieces.iter().any(|p| p.is_string);
    for &i in &kept {
        let alignment = pieces[i].alignment;
        let offset = alignment.align_up(u64::from(next_offset)) as u32;
        let len = pieces[i].bytes.len() as u32;
        next_offset = if string_class {
            offset + len
        } else {
            offset + alignment.align_up(u64::from(len)) as u32
        };
        new_strings.push(BucketString {
            bytes: pieces[i].bytes,
            offset,
            alignment,
            is_string: pieces[i].is_string,
            entsize: pieces[i].entsize,
            first_input: pieces[i].first_input,
        });
        laid_out[i] = Some(offset);
    }

    fn root_offset(pieces: &[TailMergePiece<'_>], laid_out: &[Option<u32>], mut i: usize) -> u32 {
        let mut delta = 0u32;
        while let Some(host) = pieces[i].absorbed_by {
            delta += (pieces[host].bytes.len() - pieces[i].bytes.len()) as u32;
            i = host;
        }
        laid_out[i].unwrap() + delta
    }

    for i in 0..pieces.len() {
        let new_offset = root_offset(&pieces, &laid_out, i);
        let old = BucketOffset::new(pieces[i].old_offset, pieces[i].old_bucket)?;
        let new = BucketOffset::new(new_offset, dest_bucket)?;
        if old.0 != new.0 {
            tail_remap.insert(old.0, new);
        }
    }

    for bucket in buckets.iter_mut().skip(class.base).take(class.count) {
        bucket.strings.clear();
        bucket.next_offset = 0;
    }
    buckets[dest_bucket].strings = new_strings;
    buckets[dest_bucket].next_offset = next_offset;
    Ok(())
}

/// A string from a string-merge section. Includes the null terminator.
/// Equality is by content only; alignment is upgraded to the max of all
/// occurrences (bfd 2.46 `sec_merge_hash_lookup`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct MergeString<'data> {
    bytes: &'data [u8],
    alignment: alignment::Alignment,
    is_string: bool,
    entsize: u32,
}

impl PartialEq for MergeString<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
            && self.is_string == other.is_string
            && self.entsize == other.entsize
    }
}

impl Eq for MergeString<'_> {}

/// A merged string together with its offset in the hash bucket. Trailing padding up to the
/// string's alignment is not stored; the writer zeros the bucket then copies `bytes` at `offset`.
struct BucketString<'data> {
    bytes: &'data [u8],
    offset: u32,
    alignment: alignment::Alignment,
    is_string: bool,
    entsize: u32,
    first_input: LinearInputOffset,
}

#[derive(Clone, Copy, Debug)]
struct StringPlacement {
    offset: u32,
    strings_idx: u32,
}

/// The addresses of the start of the merged strings for each output section.
#[derive(Debug)]
pub(crate) struct MergedStringStartAddresses {
    addresses: OutputSectionMap<[u64; MERGE_STRING_BUCKETS]>,
}

/// A section containing null terminated strings post-merging.
#[derive(derive_more::Debug)]
pub(crate) struct MergedStringsSection<'data> {
    /// The buckets based on the hash value of the input string.
    pub(crate) buckets: Vec<MergeStringsSectionBucket<'data>>,

    /// The byte offset of each bucket in the final section.
    bucket_offsets: [u64; MERGE_STRING_BUCKETS],

    /// Map from input offsets to output offsets.
    #[debug(skip)]
    string_offsets: OffsetMap<BucketOffset, MAP_BLOCK_SIZE>,

    /// Offsets of strings that didn't fit in `string_offsets`.
    overflowed_string_offsets: HashMap<LinearInputOffset, BucketOffset>,

    /// After tail-merging, maps hash-bucket `BucketOffset` values to the linear
    /// class-pool offset. Missing keys are left unchanged.
    #[debug(skip)]
    tail_remap: HashMap<u32, BucketOffset>,

    class_buckets: Vec<MergeClassBuckets>,
    /// Tail-merged size of each class before VMA / inter-class padding.
    class_unpadded: Vec<u32>,
}

impl Default for MergedStringsSection<'_> {
    fn default() -> Self {
        Self {
            buckets: Default::default(),
            bucket_offsets: [0; MERGE_STRING_BUCKETS],
            string_offsets: Default::default(),
            overflowed_string_offsets: HashMap::new(),
            tail_remap: HashMap::new(),
            class_buckets: Vec::new(),
            class_unpadded: Vec::new(),
        }
    }
}

#[derive(derive_more::Debug, Default)]
pub(crate) struct MergeStringsSectionBucket<'data> {
    index: usize,

    /// Input sections need to be added to a bucket in deterministic order, otherwise we'll get
    /// non-deterministic results. This is the index of the next input group that should be added.
    next_input_group_index: usize,

    /// The strings in this section, in order. Includes null terminators. Offsets may skip
    /// padding so that each string is aligned to its input section's `sh_addralign`.
    /// TODO: Debug
    #[debug(skip)]
    strings: Vec<BucketString<'data>>,

    /// The offset within the section of the next string to be added, or if we're done adding
    /// things, then this is the size of the output section.
    next_offset: u32,

    /// The total size of all added strings, used for statistics.
    input_string_byte_size: usize,

    /// The total number of all added strings, used for statistics.
    input_string_count: usize,

    /// The offsets of each string in the output section, keyed by the string contents.
    string_offsets: PassThroughHashMap<MergeString<'data>, StringPlacement>,
}

/// Merges identical strings from all loaded objects where those strings are from input sections
/// that are marked with both the SHF_MERGE and SHF_STRINGS flags.
pub(crate) fn merge_strings<'data, P: Platform>(
    inputs: &StringMergeInputs<'data>,
    output_sections: &OutputSections<P>,
    args: &P::Args,
) -> Result<OutputSectionMap<MergedStringsSection<'data>>> {
    timing_phase!("Merge strings");

    let mut output_string_sections = output_sections.new_section_map::<MergedStringsSection>();

    let num_threads = rayon::current_num_threads();
    let split_parallelism = args.common().numeric_experiment(
        Experiment::MergeStringSplitParallelism,
        (num_threads as u64).min(MAX_SPLIT_PARALLELISM),
    ) as usize;

    let reuse_pool = ReusePool::new(MERGE_STRING_BUCKETS * split_parallelism);

    inputs
        .input_sections_by_output
        .try_for_each(|section_id, input_sections| {
            // We later create ArrayQueues with capacity for all input sections and ArrayQueue
            // panics if asked for zero capacity. Also, spawning tasks and all the other
            // work we do here would be a waste if we have no input sections.
            if input_sections.is_empty() {
                return Ok(());
            }

            verbose_timing_phase!(
                "Merge section",
                section_name = output_sections.display_name(section_id)
            );

            let output_section = output_string_sections.get_mut(section_id);
            output_section.add_input_sections(input_sections, &reuse_pool, args)?;

            assert_eq!(
                reuse_pool.available.load(Ordering::Relaxed),
                reuse_pool.capacity,
            );

            Ok(())
        })?;

    output_string_sections.for_each(|section_id, sec| {
        if sec.len() > 0 {
            tracing::debug!(target: "metrics",
                section = %output_sections.display_name(section_id),
                string_count = sec.string_count(),
                byte_size = sec.len(),
                input_string_count = sec.input_string_count(),
                input_string_byte_size = sec.input_string_byte_size(),
                output_map_overflow = sec.overflowed_string_offsets.len(),
                "merge_strings");
        }
    });

    // Dropping our ReusePool can take a little while, do it in the background while we continue
    // with other work.
    rayon::spawn(|| drop(reuse_pool));

    Ok(output_string_sections)
}

impl<'data> StringMergeInputs<'data> {
    pub(crate) fn new<P: Platform>(
        resolved: &mut [ResolvedGroup<'data, P>],
        section_part_ids: &[crate::part_id::PartId],
        output_sections: &OutputSections<P>,
    ) -> Result<Self> {
        Ok(Self {
            input_sections_by_output: group_merge_string_sections_by_output(
                resolved,
                section_part_ids,
                output_sections,
            )?,
        })
    }
}

// Gather up all the string-merge sections, grouping them by their output section ID. We return a
// reference to the `MergeStringsFileSection` rather than copying it because it appears to be
// faster.
fn group_merge_string_sections_by_output<'data, P: Platform>(
    resolved: &mut [ResolvedGroup<'data, P>],
    section_part_ids: &[crate::part_id::PartId],
    output_sections: &OutputSections<P>,
) -> Result<OutputSectionMap<Vec<StringMergeInputSection<'data>>>> {
    verbose_timing_phase!("Find merge sectionns");

    let mut input_sections = output_sections.new_section_map::<Vec<StringMergeInputSection>>();

    let mut starting_offsets = output_sections.new_section_map::<LinearInputOffset>();

    for group in resolved {
        for file in &mut group.files {
            let ResolvedFile::Object(obj) = file else {
                continue;
            };
            for extra in &obj.string_merge_extras {
                let SectionSlot::MergeStrings(sec) = &mut obj.sections[extra.index.0] else {
                    bail!("Internal error: expected SectionSlot::MergeStrings");
                };

                let part_id =
                    section_part_ids[obj.section_id_range.start().as_usize() + extra.index.0];
                let section_id = part_id.output_section_id::<P>();
                let starting_offset = starting_offsets.get_mut(section_id);
                sec.start_input_offset = *starting_offset;

                input_sections
                    .get_mut(section_id)
                    .push(StringMergeInputSection {
                        section_data: extra.section_data,
                        start_input_offset: *starting_offset,
                        is_string: extra.is_strings,
                        alignment: extra.alignment,
                        entsize: extra.entsize,
                    });

                *starting_offset = *starting_offset
                    + (extra.section_data.len() as u64).next_multiple_of(MAP_BLOCK_SIZE);
            }
        }
    }

    Ok(input_sections)
}

struct StringToMerge<'data, 'offsets> {
    string: PreHashed<MergeString<'data>>,
    offset_out: OffsetOut<'offsets>,
    input_offset: LinearInputOffset,
}

/// A place where we'll store the `BucketOffset` of the string once known.
enum OffsetOut<'offsets> {
    InShard(&'offsets mut BucketOffset),
    Overflow(LinearInputOffset),
}

/// A group of input sections that we'll process together. Grouping input sections allows us to
/// reduce some overheads by doing some bookkeeping per-group rather than per input section.
struct SectionGroup<'data, 'offsets, 'sections> {
    index: usize,
    sections: &'sections [StringMergeInputSection<'data>],
    offsets_shard: sharded_offset_map::Shard<'offsets, BucketOffset, MAP_BLOCK_SIZE>,

    /// Restrict to just strings that start within the specified range.
    range: Range<LinearInputOffset>,
}

/// Split an input section into strings and hash those strings, collecting the results into
/// buckets based on the string hashes.
fn process_input_section<'data, 'offsets>(
    input_section: &StringMergeInputSection<'data>,
    buckets: &mut [Vec<StringToMerge<'data, 'offsets>>; MERGE_STRING_BUCKETS],
    offsets_shard: &mut sharded_offset_map::Shard<'offsets, BucketOffset, MAP_BLOCK_SIZE>,
    range: &Range<LinearInputOffset>,
    classes: &[MergeClassBuckets],
) -> Result {
    let mut input_offset = input_section.start_input_offset;
    let mut remaining = input_section.section_data;
    if range.start > input_offset {
        // Non-string merge sections should never be split.
        debug_assert!(input_section.is_string);

        let offset_in_section = (range.start - input_offset) as usize;
        let advance = if remaining[offset_in_section - 1] == 0 {
            // Our range started just after a null character, so we're already at the start of a
            // string.
            offset_in_section
        } else {
            // Our range start is part way through a string, find end of the string and start from
            // there.
            memchr::memchr(0, &remaining[offset_in_section..])
                .map_or(remaining.len(), |null_offset| {
                    offset_in_section + null_offset + 1
                })
        };
        input_offset = input_offset + advance as u64;
        remaining = &remaining[advance..];
    }

    let mut insert_data = |data: PreHashed<MergeString<'data>>,
                           input_offset: &mut LinearInputOffset| {
        // Insert 0, then we'll update it later once we know the output offset. We do the
        // initial insertion now since insertions need to happen in sequential order, whereas by
        // the time we know the output offset, we're processing just a single bucket.

        let offset_key = match offsets_shard.insert(input_offset.0, BucketOffset(0)) {
            Ok(offset_in_shard) => OffsetOut::InShard(offset_in_shard),
            Err(_) => OffsetOut::Overflow(*input_offset),
        };
        buckets[merge_bucket_index(data.hash(), input_section, classes)].push(StringToMerge {
            string: data,
            offset_out: offset_key,
            input_offset: *input_offset,
        });
        *input_offset = *input_offset + data.bytes.len() as u64;
    };

    // Non-string SHF_MERGE: split into `entsize` units when that is a real constant
    // width (`.rodata.cst8` / `.rodata.cst16`). entsize 0 or 1 keeps the whole section
    // as one slice so existing byte-blob merge tests stay intact.
    if !input_section.is_string {
        let entsize = input_section.entsize as usize;
        if entsize > 1 && remaining.len().is_multiple_of(entsize) {
            while !remaining.is_empty() && input_offset < range.end {
                let unit = MergeString::take_sized_hashed(
                    &mut remaining,
                    input_section.layout_alignment(),
                    entsize,
                    false,
                );
                insert_data(unit, &mut input_offset);
            }
        } else {
            let section_data =
                MergeString::take_hashed(&mut remaining, input_section.alignment, false);
            insert_data(section_data, &mut input_offset);
        }
        return Ok(());
    }

    // String section, so split at null terminators. Padding NULs are empty
    // strings (bfd `record_section`); their lower entity alignment is what
    // makes GNU ld use `strrevcmp` rather than `strrevcmp_align`.
    while !remaining.is_empty() && input_offset < range.end {
        let in_sec = input_offset - input_section.start_input_offset;
        let elt_align = entity_alignment(in_sec, input_section.alignment);
        let string =
            MergeString::take_string_hashed(&mut remaining, elt_align, input_section.entsize)?;
        insert_data(string, &mut input_offset);
    }

    Ok(())
}

impl<'data> MergedStringsSection<'data> {
    fn add_input_sections(
        &mut self,
        input_sections: &[StringMergeInputSection<'data>],
        reuse_pool: &ReusePool,
        args: &impl platform::Args,
    ) -> Result {
        let mut resources =
            create_split_resources(&mut self.string_offsets, input_sections, reuse_pool, args);

        rayon::in_place_scope(|s| {
            // Spawn some number of tasks to process input section groups. As these tasks complete,
            // they'll spawn bucket processing tasks to take those inputs. As the bucket processing
            // tasks complete, they will, as capacity permits, spawn additional input processing
            // tasks. This continues until the last inputs and the last buckets have been processed.
            try_spawn_input_processing(&resources, s);
        });

        // Check if we got any errors. We only look at the first error.
        if let Some(error) = resources.errors.pop() {
            return Err(error);
        }

        {
            verbose_timing_phase!("Handle overflows");

            // Handle any offsets that didn't fit in their respective blocks in the offset map.
            let overflow = core::mem::take(&mut resources.overflowed_offsets);
            overflow
                .into_iter()
                .flat_map(|cell| cell.into_inner())
                .for_each(|o| {
                    self.overflowed_string_offsets.insert(o.input, o.output);
                });
        }

        verbose_timing_phase!("Finalise merged section");

        // Move our buckets out of `resources` and convert it to a regular Vec.
        let mut buckets = resources
            .finished_buckets
            .into_iter()
            .map(|b| *b)
            .collect_vec();
        buckets.sort_by_key(|b| b.index);

        apply_string_tail_merge(&mut buckets, &resources.class_buckets, &mut self.tail_remap)?;
        let class_unpadded: Vec<u32> = resources
            .class_buckets
            .iter()
            .map(|c| buckets[c.base].next_offset)
            .collect();
        self.class_buckets = resources.class_buckets.clone();
        self.class_unpadded = class_unpadded;
        self.buckets = buckets;
        let leading = pad_merge_buckets(
            &mut self.buckets,
            &self.class_buckets,
            0,
            &self.class_unpadded,
        );
        self.bucket_offsets[0] = u64::from(leading);
        for i in 1..MERGE_STRING_BUCKETS {
            self.bucket_offsets[i] =
                self.bucket_offsets[i - 1] + u64::from(self.buckets[i - 1].next_offset);
        }

        resources.finished_shards.into_iter().for_each(|shard| {
            resources
                .offset_writer
                .return_shard(shard.into_inner().unwrap());
        });

        Ok(())
    }

    /// Returns the size in bytes of this section.
    pub(crate) fn len(&self) -> u64 {
        self.buckets
            .last()
            .map(|last_bucket| {
                u64::from(last_bucket.next_offset) + self.bucket_offsets[last_bucket.index]
            })
            .unwrap_or_default()
    }

    pub(crate) fn input_string_byte_size(&self) -> usize {
        self.buckets.iter().map(|b| b.input_string_byte_size).sum()
    }

    pub(crate) fn input_string_count(&self) -> usize {
        self.buckets.iter().map(|b| b.input_string_count).sum()
    }

    pub(crate) fn string_count(&self) -> usize {
        self.buckets.iter().map(|b| b.strings.len()).sum()
    }

    fn recompute_bucket_offsets(&mut self, start_vma: u64) {
        if self.class_buckets.is_empty() {
            return;
        }
        let leading = pad_merge_buckets(
            &mut self.buckets,
            &self.class_buckets,
            start_vma,
            &self.class_unpadded,
        );
        self.bucket_offsets[0] = u64::from(leading);
        for i in 1..MERGE_STRING_BUCKETS {
            self.bucket_offsets[i] =
                self.bucket_offsets[i - 1] + u64::from(self.buckets[i - 1].next_offset);
        }
    }

    /// Re-pad merge classes so each starts at an aligned absolute VMA. Returns
    /// the change in section size (new minus old).
    pub(crate) fn repad_to_vma(&mut self, start_vma: u64) -> i64 {
        let old = self.len();
        self.recompute_bucket_offsets(start_vma);
        self.len() as i64 - old as i64
    }

    pub(crate) fn leading_pad(&self) -> usize {
        self.bucket_offsets[0] as usize
    }
}

struct SplitResources<'data, 'offsets, 'scope> {
    /// The number of input groups that we're processing. This is used so that we can know when
    /// we've processed all input groups for a particular hash bucket.
    num_input_groups: usize,

    /// Groups that we haven't yet processed in phase 1.
    unprocessed: ArrayQueue<SectionGroup<'data, 'offsets, 'scope>>,

    // The shards that we've finished processing in their correct order. Note, this `AtomicCell`
    // isn't lock-free, since the shard is larger than a usize. This doesn't seem to make any
    // measurable difference to performance for our use-case.
    finished_shards:
        Vec<AtomicCell<Option<sharded_offset_map::Shard<'offsets, BucketOffset, MAP_BLOCK_SIZE>>>>,

    /// Indexed by group and bucket. See `string_bucket_offset` for computation.
    strings_by_bucket_and_group: Vec<Mutex<StringsSlot<'data, 'offsets>>>,

    /// Hash buckets that we've finished with. These have had all input groups applied.
    finished_buckets: ArrayQueue<Box<MergeStringsSectionBucket<'data>>>,

    /// Bucket ranges for each merge class (string alignment vs constant pool).
    class_buckets: Vec<MergeClassBuckets>,

    offset_writer: sharded_offset_map::ShardedWriter<'offsets, BucketOffset, MAP_BLOCK_SIZE>,

    /// Any offsets that couldn't fit in the offset map due to too many strings within a block.
    overflowed_offsets: ThreadLocal<RefCell<Vec<OverflowedOffset>>>,

    errors: ArrayQueue<crate::error::Error>,

    reuse_pool: &'scope ReusePool,
}

fn string_bucket_offset(input: usize, bucket: usize) -> usize {
    input * MERGE_STRING_BUCKETS + bucket
}

impl<'scope, 'data: 'scope, 'offsets> SplitResources<'data, 'offsets, 'scope> {
    fn swap_strings_slot(
        &self,
        input: usize,
        bucket: usize,
        slot: StringsSlot<'data, 'offsets>,
    ) -> StringsSlot<'data, 'offsets> {
        let mut lock = self.strings_by_bucket_and_group[string_bucket_offset(input, bucket)]
            .lock()
            .unwrap();
        replace(&mut lock, slot)
    }
}

// Spawn as many input-processing tasks as allowed.
fn try_spawn_input_processing<'scope>(
    resources: &'scope SplitResources<'_, '_, '_>,
    scope: &Scope<'scope>,
) {
    loop {
        let Ok(mut reservation) = resources.reuse_pool.try_reserve(MERGE_STRING_BUCKETS) else {
            return;
        };

        scope.spawn(|scope| {
            if let Some(input_section) = resources.unprocessed.pop()
                && let Err(error) =
                    process_input_section_group(resources, input_section, scope, &mut reservation)
            {
                let _ = resources.errors.push(error);
            }

            resources.reuse_pool.unreserve(reservation);
        });
    }
}

enum StringsSlot<'data, 'offsets> {
    Empty,
    WaitingForStrings(Box<MergeStringsSectionBucket<'data>>),
    Strings(Vec<StringToMerge<'data, 'offsets>>),
}

fn create_split_resources<'data, 'offsets, 'scope>(
    string_offsets: &'offsets mut OffsetMap<BucketOffset, MAP_BLOCK_SIZE>,
    input_sections: &'scope [StringMergeInputSection<'data>],
    reuse_pool: &'scope ReusePool,
    args: &impl platform::Args,
) -> SplitResources<'data, 'offsets, 'scope> {
    verbose_timing_phase!("Create input section groups");

    let input_size = total_input_size(input_sections);
    let mut offset_writer = string_offsets.start_sharded_write(input_size.0);

    let target_group_size = args
        .common()
        .numeric_experiment(
            Experiment::MergeStringMinGroupBytes,
            TARGET_GROUP_SIZE_BYTES,
        )
        .next_multiple_of(MAP_BLOCK_SIZE) as usize;

    let groups = split_sections(input_sections, &mut offset_writer, target_group_size);

    let unprocessed: ArrayQueue<SectionGroup> = ArrayQueue::new(groups.len());
    for group in groups {
        let _ = unprocessed.push(group);
    }

    let num_groups = unprocessed.len();
    let mut strings_by_bucket_and_group = Vec::new();
    strings_by_bucket_and_group.resize_with(num_groups * MERGE_STRING_BUCKETS, || {
        Mutex::new(StringsSlot::Empty)
    });

    let mut finished_shards = Vec::new();
    finished_shards.resize_with(num_groups, || AtomicCell::new(None));

    let resources = SplitResources {
        num_input_groups: unprocessed.len(),
        unprocessed,
        strings_by_bucket_and_group,
        finished_buckets: ArrayQueue::new(MERGE_STRING_BUCKETS),
        finished_shards,
        overflowed_offsets: ThreadLocal::new(),
        offset_writer,
        errors: ArrayQueue::new(1),
        reuse_pool,
        class_buckets: build_merge_class_buckets(input_sections),
    };

    (0..MERGE_STRING_BUCKETS).for_each(|i| {
        resources.swap_strings_slot(
            0,
            i,
            StringsSlot::WaitingForStrings(Box::new(MergeStringsSectionBucket::new(i))),
        );
    });

    resources
}

/// Split `sections` into slices of at most `size`. A single input section might be split into
/// multiple groups, or a group might contain multiple input sections. The last slice may be
/// smaller. If the sections are string sections, then the split will occur after exactly size bytes
/// unless we run out of sections first. If a section is a non-string merge section, then the whole
/// section will be taken regardless of size.
fn split_sections<'data, 'offsets, 'sections>(
    sections: &'sections [StringMergeInputSection<'data>],
    offset_writer: &mut ShardedWriter<'offsets, BucketOffset, MAP_BLOCK_SIZE>,
    size: usize,
) -> Vec<SectionGroup<'data, 'offsets, 'sections>> {
    assert!(size.is_multiple_of(MAP_BLOCK_SIZE as usize));

    let mut result = Vec::new();

    let mut section_index = 0;
    let mut offset_in_section = 0;

    while section_index < sections.len() {
        // Remaining needs to be signed, since if we encounter non-string merge sections, we'll need
        // to take the entire section, which may cause us to go negative.
        let mut remaining = size as isize;
        let start_section_index = section_index;
        let first_section_start_offset = offset_in_section;
        let mut end_section = false;

        // Iterate through sections until we fill `size` bytes
        while remaining > 0 {
            let sec = &sections[section_index];
            let available = (sec.padded_len() - offset_in_section) as isize;

            if available > remaining && sec.is_string {
                // Still some of this section left for the next group, so don't advance.
                offset_in_section += remaining as usize;
                remaining = 0;
            } else {
                remaining -= available;
                if remaining <= 0 || section_index + 1 == sections.len() {
                    offset_in_section += available as usize;
                    end_section = true;
                    break;
                }
                section_index += 1;
                offset_in_section = 0;
            }
        }

        let index = result.len();
        let group_size = size as isize - remaining;

        let linear_start =
            sections[start_section_index].start_input_offset + first_section_start_offset as u64;
        let linear_end = sections[section_index].start_input_offset + offset_in_section as u64;

        let offsets_shard = offset_writer.take_shard(group_size as u64);

        debug_assert_eq!(linear_start.0, offsets_shard.base());
        debug_assert_eq!(linear_end.0, offsets_shard.base() + offsets_shard.len());

        result.push(SectionGroup {
            sections: &sections[start_section_index..=section_index],
            range: linear_start..linear_end,
            index,
            offsets_shard,
        });

        if end_section {
            section_index += 1;
            offset_in_section = 0;
        }
    }

    result
}

struct ReusePool {
    string_vecs: ArrayQueue<Vec<StringToMerge<'static, 'static>>>,

    capacity: usize,

    /// Number of Vecs that haven't yet been reserved.
    available: AtomicUsize,
}

/// Holds instances of data structures that we reuse where possible. This allows us to reduce the
/// number of separate heap allocations we make.
impl ReusePool {
    fn new(capacity: usize) -> Self {
        Self {
            string_vecs: ArrayQueue::new(capacity),
            capacity,
            available: AtomicUsize::new(capacity),
        }
    }

    fn take_string_merge_vec<'data, 'offsets>(
        &self,
        reservation: &mut PoolReservation,
    ) -> Vec<StringToMerge<'data, 'offsets>> {
        reservation.remaining = reservation.remaining.checked_sub(1).unwrap();
        self.string_vecs
            .pop()
            .map_or_else(|| Vec::with_capacity(1024), reuse_vec)
    }

    fn return_strings_to_merge(&self, strings_to_merge: Vec<StringToMerge<'_, '_>>) {
        let r = self.string_vecs.push(reuse_vec(strings_to_merge));
        assert!(r.is_ok());

        self.available.fetch_add(1, Ordering::Relaxed);
    }

    /// Attempt to reserve the specified number of Vecs. Fails if there isn't at least that many
    /// already available.
    fn try_reserve(&self, num_vecs: usize) -> Result<PoolReservation, ()> {
        let available = self.available.load(Ordering::Relaxed);
        if available < num_vecs {
            return Err(());
        }

        if self
            .available
            .compare_exchange(
                available,
                available - num_vecs,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Err(());
        }

        Ok(PoolReservation {
            remaining: num_vecs,
        })
    }

    #[allow(clippy::needless_pass_by_value)]
    fn unreserve(&self, reservation: PoolReservation) {
        if reservation.remaining == 0 {
            return;
        }
        self.available
            .fetch_add(reservation.remaining, Ordering::Relaxed);
    }
}

struct PoolReservation {
    remaining: usize,
}

/// Returns the total size of our input sections. Each input section's size is rounded up to a block
/// size.
fn total_input_size(input_sections: &[StringMergeInputSection<'_>]) -> LinearInputOffset {
    input_sections
        .last()
        .map(|sec| {
            sec.start_input_offset
                + (sec.section_data.len() as u64).next_multiple_of(MAP_BLOCK_SIZE)
        })
        .unwrap_or_default()
}

/// Perform initial processing of the input sections in a group.
fn process_input_section_group<'data, 'offsets, 'scope>(
    resources: &'scope SplitResources<'data, 'offsets, '_>,
    mut group_in: SectionGroup<'data, 'offsets, 'scope>,
    scope: &Scope<'scope>,
    reservation: &mut PoolReservation,
) -> Result {
    verbose_timing_phase!("Split and hash");

    let mut buckets: [Vec<StringToMerge<'data, 'offsets>>; MERGE_STRING_BUCKETS] = [();
        MERGE_STRING_BUCKETS]
        .map(|()| resources.reuse_pool.take_string_merge_vec(reservation));

    for section in group_in.sections {
        process_input_section(
            section,
            &mut buckets,
            &mut group_in.offsets_shard,
            &group_in.range,
            resources.class_buckets.as_slice(),
        )?;
    }

    group_in.offsets_shard.finish();
    resources.finished_shards[group_in.index].store(Some(group_in.offsets_shard));

    for (i, bucket_out) in buckets.iter_mut().enumerate() {
        let prev_slot =
            resources.swap_strings_slot(group_in.index, i, StringsSlot::Strings(take(bucket_out)));
        if let StringsSlot::WaitingForStrings(bucket) = prev_slot {
            scope.spawn(|scope| {
                if let Err(error) = work_with_bucket(resources, bucket, scope) {
                    let _ = resources.errors.push(error);
                }
            });
        }
    }

    Ok(())
}

/// Do all work possible with the supplied bucket then return it to an appropriate location.
fn work_with_bucket<'data, 'scope>(
    resources: &'scope SplitResources<'data, '_, '_>,
    mut bucket: Box<MergeStringsSectionBucket<'data>>,
    scope: &Scope<'scope>,
) -> Result {
    verbose_timing_phase!("Bucket strings");

    let mut overflowed_offsets = resources.overflowed_offsets.get_or_default().borrow_mut();

    while bucket.next_input_group_index < resources.num_input_groups {
        let mut strings_to_merge = {
            let group_index = bucket.next_input_group_index;

            let mut lock = resources.strings_by_bucket_and_group
                [string_bucket_offset(group_index, bucket.index)]
            .lock()
            .unwrap();

            let slot = replace(&mut *lock, StringsSlot::Empty);
            let StringsSlot::Strings(strings) = slot else {
                *lock = StringsSlot::WaitingForStrings(bucket);
                return Ok(());
            };

            strings
        };

        bucket.process_split_output(&mut strings_to_merge, &mut overflowed_offsets)?;

        resources
            .reuse_pool
            .return_strings_to_merge(strings_to_merge);

        try_spawn_input_processing(resources, scope);

        // Advance to the next input for this bucket.
        bucket.next_input_group_index += 1;
    }

    // This bucket has now processed all input sections, so it's done.
    let _ = resources.finished_buckets.push(bucket);
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
struct BucketOffset(u32);

struct OverflowedOffset {
    input: LinearInputOffset,
    output: BucketOffset,
}

impl BucketOffset {
    fn new(offset: u32, bucket: usize) -> Result<Self> {
        if offset >= 1 << (32 - MERGE_STRING_BUCKET_BITS) {
            bail!("Merge-string bucket too large");
        }
        Ok(BucketOffset(
            ((bucket as u32) << (32 - MERGE_STRING_BUCKET_BITS)) | offset,
        ))
    }

    fn bucket(self) -> usize {
        (self.0 >> (32 - MERGE_STRING_BUCKET_BITS)) as usize
    }

    fn offset_in_bucket(self) -> u64 {
        u64::from(self.0 & ((1 << (32 - MERGE_STRING_BUCKET_BITS)) - 1))
    }
}

impl<'data> MergeStringsSectionBucket<'data> {
    fn process_split_output(
        &mut self,
        strings_to_merge: &mut [StringToMerge<'data, '_>],
        overflowed_offsets: &mut Vec<OverflowedOffset>,
    ) -> Result {
        let bucket_index = self.index;
        for string in strings_to_merge {
            let offset_in_bucket =
                self.add_string(string.string, bucket_index, string.input_offset)?;
            match &mut string.offset_out {
                OffsetOut::InShard(o) => {
                    **o = offset_in_bucket;
                }
                OffsetOut::Overflow(linear_input_offset) => {
                    overflowed_offsets.push(OverflowedOffset {
                        input: *linear_input_offset,
                        output: offset_in_bucket,
                    });
                }
            }
        }
        Ok(())
    }

    /// Adds `string`, deduplicating by content. Alignment is upgraded to the max
    /// of all occurrences (bfd 2.46). Temporary bucket packing uses `align_up(len)`;
    /// string classes are re-laid out after tail-merge.
    fn add_string(
        &mut self,
        string: PreHashed<MergeString<'data>>,
        bucket_index: usize,
        input_offset: LinearInputOffset,
    ) -> Result<BucketOffset> {
        self.input_string_byte_size += string.bytes.len();
        self.input_string_count += 1;
        if let Some(&StringPlacement {
            offset,
            strings_idx,
        }) = self.string_offsets.get(&string)
        {
            let existing = &mut self.strings[strings_idx as usize];
            if string.alignment > existing.alignment {
                existing.alignment = string.alignment;
            }
            if input_offset < existing.first_input {
                existing.first_input = input_offset;
            }
            return BucketOffset::new(offset, bucket_index);
        }
        let alignment = string.alignment;
        let offset = alignment.align_up(u64::from(self.next_offset)) as u32;
        let padded_len = alignment.align_up(string.bytes.len() as u64) as u32;
        self.next_offset = offset + padded_len;
        let strings_idx = self.strings.len() as u32;
        self.strings.push(BucketString {
            bytes: string.bytes,
            offset,
            alignment,
            is_string: string.is_string,
            entsize: string.entsize,
            first_input: input_offset,
        });
        self.string_offsets.insert(
            string,
            StringPlacement {
                offset,
                strings_idx,
            },
        );
        BucketOffset::new(offset, bucket_index)
    }

    fn new(i: usize) -> Self {
        Self {
            index: i,
            ..Default::default()
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.next_offset as usize
    }

    /// Writes this bucket into `buffer`, which must be exactly `self.len()` bytes. Padding
    /// between strings (and at the end of the bucket) is left as zero.
    pub(crate) fn write_to(&self, buffer: &mut [u8]) {
        debug_assert_eq!(buffer.len(), self.len());
        buffer.fill(0);
        for string in &self.strings {
            let start = string.offset as usize;
            buffer[start..start + string.bytes.len()].copy_from_slice(string.bytes);
        }
    }
}

impl<'data> MergeString<'data> {
    /// Takes from `source` up to the next null terminator. Returns a prehashed reference to what
    /// was taken.
    pub(crate) fn take_string_hashed(
        source: &mut &'data [u8],
        alignment: alignment::Alignment,
        entsize: u64,
    ) -> Result<PreHashed<MergeString<'data>>> {
        let entsize = entsize.max(1) as usize;
        let len = if entsize == 1 {
            memchr::memchr(0, source).map(|i| i + 1)
        } else {
            let mut i = 0;
            loop {
                if i + entsize > source.len() {
                    break None;
                }
                if source[i..i + entsize].iter().all(|b| *b == 0) {
                    break Some(i + entsize);
                }
                i += entsize;
            }
        }
        .context("String in merge-string section is not null-terminated")?;
        let (bytes, rest) = source.split_at(len);
        let entsize = entsize as u32;
        let hash = hash_merge_string(bytes, true, entsize);
        *source = rest;
        Ok(PreHashed::new(
            MergeString {
                bytes,
                alignment,
                is_string: true,
                entsize,
            },
            hash,
        ))
    }

    /// Takes `size` bytes (or the remainder) from `source`.
    pub(crate) fn take_sized_hashed(
        source: &mut &'data [u8],
        alignment: alignment::Alignment,
        size: usize,
        is_string: bool,
    ) -> PreHashed<MergeString<'data>> {
        let take_n = size.min(source.len());
        let (bytes, rest) = source.split_at(take_n);
        *source = rest;
        let entsize = size as u32;
        let hash = hash_merge_string(bytes, is_string, entsize);
        PreHashed::new(
            MergeString {
                bytes,
                alignment,
                is_string,
                entsize,
            },
            hash,
        )
    }

    /// Takes the whole `source`. Returns a prehashed reference to what was taken.
    pub(crate) fn take_hashed(
        source: &mut &'data [u8],
        alignment: alignment::Alignment,
        is_string: bool,
    ) -> PreHashed<MergeString<'data>> {
        let bytes = take(source);
        let hash = hash_merge_string(bytes, is_string, 1);
        PreHashed::new(
            MergeString {
                bytes,
                alignment,
                is_string,
                entsize: 1,
            },
            hash,
        )
    }
}

fn hash_merge_string(bytes: &[u8], is_string: bool, entsize: u32) -> u64 {
    crate::hash::hash_bytes(bytes)
        ^ if is_string { 0 } else { 0x517c_c1b7_2722_0a95 }
        ^ 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(u64::from(entsize))
}

/// Looks for a merged string at `symbol_index` + `addend` in the input and if found, returns its
/// address in the output.
#[inline(always)]
pub(crate) fn get_merged_string_output_address<'data, P: Platform>(
    symbol_index: object::SymbolIndex,
    addend: i64,
    object: &P::File<'data>,
    sections: &[SectionSlot],
    section_part_ids: &[PartId],
    section_id_range: SectionIdRange,
    merged_strings: &OutputSectionMap<MergedStringsSection>,
    merged_string_start_addresses: &MergedStringStartAddresses,
    zero_unnamed: bool,
) -> Result<Option<u64>> {
    let symbol = object.symbol(symbol_index)?;
    let Some(section_index) = object.symbol_section(symbol, symbol_index)? else {
        return Ok(None);
    };

    let input_section_id = section_id_range.input_to_id(section_index);

    let SectionSlot::MergeStrings(merge_slot) = &sections[section_index.0] else {
        return Ok(None);
    };
    let mut input_offset = symbol.value();

    // When we reference data in a string-merge section via a named symbol, we determine which
    // string we're referencing without taking the addend into account, then apply the addend
    // afterward. However when the reference is to a section (a symbol without a name), we take the
    // addend into account up-front before we determine which string we're pointing at. This is a
    // bit weird, but seems to match what other linkers do.
    let symbol_has_name = symbol.has_name();
    if !symbol_has_name {
        // We're computing a resolution for an unnamed symbol, just use the value of 0 for now.
        // We'll compute the address later when we're processing relocations that reference the
        // section.
        if zero_unnamed {
            return Ok(Some(0));
        }
        input_offset = input_offset.wrapping_add(addend as u64);
    }

    let part_id = section_part_ids[input_section_id.as_usize()];
    let section_id = part_id.output_section_id::<P>();
    let strings_section = merged_strings.get(section_id);
    let string_offset = find_string(*merge_slot, input_offset, strings_section)?;
    let bucket_base =
        merged_string_start_addresses.addresses.get(section_id)[string_offset.bucket()];
    let mut address = bucket_base + string_offset.offset_in_bucket();
    if symbol_has_name {
        address = address.wrapping_add(addend as u64);
    }
    Ok(Some(address))
}

fn remap_tail_offset(offset: BucketOffset, section: &MergedStringsSection<'_>) -> BucketOffset {
    section.tail_remap.get(&offset.0).copied().unwrap_or(offset)
}

fn find_string(
    merge_slot: StringMergeSectionSlot,
    input_offset: u64,
    strings_section: &MergedStringsSection<'_>,
) -> Result<BucketOffset> {
    let linear_input_offset = merge_slot.start_input_offset + input_offset;
    let string_offset = strings_section
        .string_offsets
        .get(linear_input_offset.0)
        .or_else(|| {
            strings_section
                .overflowed_string_offsets
                .get(&linear_input_offset)
                .copied()
        });

    if let Some(string_offset) = string_offset {
        return Ok(remap_tail_offset(string_offset, strings_section));
    }

    // Our input offset wasn't found, so it likely points part way into a string. Search backwards
    // until we find it. It should be possible to do this more efficiently, but since we expect this
    // to be very rare, we don't bother for now.
    for i in 1..=input_offset {
        let linear_input_offset = merge_slot.start_input_offset + (input_offset - i);
        let string_offset = strings_section
            .string_offsets
            .get(linear_input_offset.0)
            .or_else(|| {
                strings_section
                    .overflowed_string_offsets
                    .get(&linear_input_offset)
                    .copied()
            });

        if let Some(string_offset) = string_offset {
            let start = remap_tail_offset(string_offset, strings_section);
            return Ok(BucketOffset(start.0 + i as u32));
        }
    }

    bail!(
        "Failed to find merge-string at offset {}",
        linear_input_offset.0
    )
}

impl MergedStringStartAddresses {
    pub(crate) fn compute<P: Platform>(
        output_sections: &OutputSections<'_, P>,
        starting_mem_offsets_by_group: &[OutputSectionPartMap<u64>],
        merge_string_sections: &OutputSectionMap<MergedStringsSection>,
    ) -> Self {
        timing_phase!("Compute merged string section start addresses");

        let mut addresses = output_sections.new_section_map_with(|| [0; MERGE_STRING_BUCKETS]);
        let internal_start_offsets = starting_mem_offsets_by_group.first().unwrap();
        merge_string_sections.for_each(|section_id, sec| {
            if !section_id.is_regular::<P>() {
                return;
            }
            // We already have the offsets of each bucket relative to the start of the section. So
            // now we just need to add the section's start address to all of these.
            let base =
                internal_start_offsets.get(section_id.part_id_with_alignment::<P>(alignment::MIN));
            let bucket_offsets_out = addresses.get_mut(section_id);
            *bucket_offsets_out = sec.bucket_offsets;
            for offset in bucket_offsets_out {
                *offset += base;
            }
        });
        Self { addresses }
    }
}

impl StringMergeInputSection<'_> {
    /// Returns the length of this section's data rounded up to the next multiple of the block size.
    fn padded_len(&self) -> usize {
        self.section_data
            .len()
            .next_multiple_of(MAP_BLOCK_SIZE as usize)
    }
}

impl std::fmt::Display for MergeString<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.bytes))
    }
}

/// Returns an empty `Vec<U>` that reuses the storage of the supplied `Vec<T>`. `T` and `U` must
/// have the same size and alignment.
fn reuse_vec<T, U>(mut v: Vec<T>) -> Vec<U> {
    debug_assert_eq!(size_of::<T>(), size_of::<U>());
    debug_assert_eq!(align_of::<T>(), align_of::<U>());
    let old_storage = v.as_ptr();
    v.clear();
    // Convert the type of the vec. This relies on a specialised implementation of `collect`. Were
    // it not for that, we'd get a new heap allocation, which would defeat the purpose.
    let u: Vec<U> = v.into_iter().map(|_| unreachable!()).collect();
    // Make sure that we actually reused the old storage.
    debug_assert_eq!(old_storage as usize, u.as_ptr() as usize);
    u
}
