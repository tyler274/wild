use crate::alignment;
use crate::error::Error;
use crate::error::Result;
use crate::hash::PassThroughHashMap;
use crate::hash::PreHashed;
use crate::output_section_map::OutputSectionMap;
use crossbeam_queue::ArrayQueue;
use crossbeam_utils::atomic::AtomicCell;
use hashbrown::HashMap;
use sharded_offset_map::OffsetMap;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use thread_local::ThreadLocal;

/// Maximum number of threads that can split and hash input sections at once. We default to allowing
/// splitting parallelism up to the number of threads, but beyond about 24 it doesn't really help.
pub(super) const MAX_SPLIT_PARALLELISM: u64 = 24;

/// How large should our chunks of input bytes be.
pub(super) const TARGET_GROUP_SIZE_BYTES: u64 = 140_000;

/// Setting this to a higher value increases the potential for parallelism of hash table population
/// and gives better cache performance. However, it also increases heap allocations. Changing this
/// value will result in a different ordering of strings within the output file.
pub(super) const MERGE_STRING_BUCKET_BITS: usize = 4;
pub(super) const MERGE_STRING_BUCKETS: usize = 1 << MERGE_STRING_BUCKET_BITS;

/// Number of input offsets to represent by a single block. A block can store up to 12 offsets. If
/// we get more than 12 offsets within a block, then we need to spill the offset to a hashmap.
/// Increasing this value decreases memory usage, however it may result in more offsets being
/// spilled to the hashmap.
pub(super) const MAP_BLOCK_SIZE: u64 = 256;

pub(crate) struct StringMergeInputs<'data> {
    pub(super) input_sections_by_output: OutputSectionMap<Vec<StringMergeInputSection<'data>>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StringMergeSectionSlot {
    /// The sum of the sizes of the input sections prior to this one with the same part ID.
    /// Populated during string merging.
    pub(super) start_input_offset: LinearInputOffset,
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
pub(super) struct LinearInputOffset(pub(super) u64);

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
pub(super) struct StringMergeInputSection<'data> {
    pub(super) section_data: &'data [u8],

    /// The sum of the sizes of the input sections prior to this one with the same `part_id`.
    pub(super) start_input_offset: LinearInputOffset,

    pub(super) is_string: bool,

    /// `sh_addralign` of the input section. Strings from different alignments are not deduped
    /// and are placed at offsets congruent to 0 modulo this alignment.
    pub(super) alignment: alignment::Alignment,

    /// `sh_entsize`. Non-string merge sections with entsize > 1 are split into that many bytes.
    pub(super) entsize: u64,
}

impl StringMergeInputSection<'_> {
    /// `.rodata.cst8` / `.cst16` / … - kept in their own merge class so their
    /// alignment does not pad strings.
    pub(super) fn is_constant_pool(self) -> bool {
        !self.is_string && self.entsize > 1
    }

    /// Alignment used when packing units of this section. GNU ld aligns
    /// `SHF_MERGE` constant-pool entities to `max(sh_addralign, sh_entsize)` when
    /// `sh_entsize` is a power of two.
    pub(super) fn layout_alignment(self) -> alignment::Alignment {
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
    pub(super) fn merge_class_key(self) -> u32 {
        if self.is_constant_pool() {
            // Distinct from string keys (alignment exponents are small).
            0x8000_0000 | ((self.entsize as u32) << 8) | u32::from(self.alignment.exponent)
        } else {
            ((self.entsize.max(1) as u32) << 8) | u32::from(self.alignment.exponent)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MergeClassBuckets {
    pub(super) key: u32,
    pub(super) base: usize,
    pub(super) count: usize,
    pub(super) pad_align: alignment::Alignment,
}
pub(super) struct TailMergePiece<'data> {
    pub(super) bytes: &'data [u8],
    pub(super) alignment: alignment::Alignment,
    pub(super) is_string: bool,
    pub(super) entsize: u32,
    pub(super) old_bucket: usize,
    pub(super) old_offset: u32,
    pub(super) first_input: LinearInputOffset,
    pub(super) absorbed_by: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MergeString<'data> {
    pub(super) bytes: &'data [u8],
    pub(super) alignment: alignment::Alignment,
    pub(super) is_string: bool,
    pub(super) entsize: u32,
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
pub(super) struct BucketString<'data> {
    pub(super) bytes: &'data [u8],
    pub(super) offset: u32,
    pub(super) alignment: alignment::Alignment,
    pub(super) is_string: bool,
    pub(super) entsize: u32,
    pub(super) first_input: LinearInputOffset,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct StringPlacement {
    pub(super) offset: u32,
    pub(super) strings_idx: u32,
}

/// The addresses of the start of the merged strings for each output section.
#[derive(Debug)]
pub(crate) struct MergedStringStartAddresses {
    pub(super) addresses: OutputSectionMap<[u64; MERGE_STRING_BUCKETS]>,
}

/// A section containing null terminated strings post-merging.
#[derive(derive_more::Debug)]
pub(crate) struct MergedStringsSection<'data> {
    /// The buckets based on the hash value of the input string.
    pub(crate) buckets: Vec<MergeStringsSectionBucket<'data>>,

    /// The byte offset of each bucket in the final section.
    pub(super) bucket_offsets: [u64; MERGE_STRING_BUCKETS],

    /// Map from input offsets to output offsets.
    #[debug(skip)]
    pub(super) string_offsets: OffsetMap<BucketOffset, MAP_BLOCK_SIZE>,

    /// Offsets of strings that didn't fit in `string_offsets`.
    pub(super) overflowed_string_offsets: HashMap<LinearInputOffset, BucketOffset>,

    /// After tail-merging, maps hash-bucket `BucketOffset` values to the linear
    /// class-pool offset. Missing keys are left unchanged.
    #[debug(skip)]
    pub(super) tail_remap: HashMap<u32, BucketOffset>,

    pub(super) class_buckets: Vec<MergeClassBuckets>,
    /// Tail-merged size of each class before VMA / inter-class padding.
    pub(super) class_unpadded: Vec<u32>,
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
    pub(super) index: usize,

    /// Input sections need to be added to a bucket in deterministic order, otherwise we'll get
    /// non-deterministic results. This is the index of the next input group that should be added.
    pub(super) next_input_group_index: usize,

    /// The strings in this section, in order. Includes null terminators. Offsets may skip
    /// padding so that each string is aligned to its input section's `sh_addralign`.
    /// TODO: Debug
    #[debug(skip)]
    pub(super) strings: Vec<BucketString<'data>>,

    /// The offset within the section of the next string to be added, or if we're done adding
    /// things, then this is the size of the output section.
    pub(super) next_offset: u32,

    /// The total size of all added strings, used for statistics.
    pub(super) input_string_byte_size: usize,

    /// The total number of all added strings, used for statistics.
    pub(super) input_string_count: usize,

    /// The offsets of each string in the output section, keyed by the string contents.
    pub(super) string_offsets: PassThroughHashMap<MergeString<'data>, StringPlacement>,
}

pub(super) struct StringToMerge<'data, 'offsets> {
    pub(super) string: PreHashed<MergeString<'data>>,
    pub(super) offset_out: OffsetOut<'offsets>,
    pub(super) input_offset: LinearInputOffset,
}

/// A place where we'll store the `BucketOffset` of the string once known.
pub(super) enum OffsetOut<'offsets> {
    InShard(&'offsets mut BucketOffset),
    Overflow(LinearInputOffset),
}

/// A group of input sections that we'll process together. Grouping input sections allows us to
/// reduce some overheads by doing some bookkeeping per-group rather than per input section.
pub(super) struct SectionGroup<'data, 'offsets, 'sections> {
    pub(super) index: usize,
    pub(super) sections: &'sections [StringMergeInputSection<'data>],
    pub(super) offsets_shard: sharded_offset_map::Shard<'offsets, BucketOffset, MAP_BLOCK_SIZE>,

    /// Restrict to just strings that start within the specified range.
    pub(super) range: Range<LinearInputOffset>,
}

/// Split an input section into strings and hash those strings, collecting the results into
pub(super) struct SplitResources<'data, 'offsets, 'scope> {
    /// The number of input groups that we're processing. This is used so that we can know when
    /// we've processed all input groups for a particular hash bucket.
    pub(super) num_input_groups: usize,

    /// Groups that we haven't yet processed in phase 1.
    pub(super) unprocessed: ArrayQueue<SectionGroup<'data, 'offsets, 'scope>>,

    // The shards that we've finished processing in their correct order. Note, this `AtomicCell`
    // isn't lock-free, since the shard is larger than a usize. This doesn't seem to make any
    // measurable difference to performance for our use-case.
    pub(super) finished_shards:
        Vec<AtomicCell<Option<sharded_offset_map::Shard<'offsets, BucketOffset, MAP_BLOCK_SIZE>>>>,

    /// Indexed by group and bucket. See `string_bucket_offset` for computation.
    pub(super) strings_by_bucket_and_group: Vec<Mutex<StringsSlot<'data, 'offsets>>>,

    /// Hash buckets that we've finished with. These have had all input groups applied.
    pub(super) finished_buckets: ArrayQueue<Box<MergeStringsSectionBucket<'data>>>,

    /// Bucket ranges for each merge class (string alignment vs constant pool).
    pub(super) class_buckets: Vec<MergeClassBuckets>,

    pub(super) offset_writer:
        sharded_offset_map::ShardedWriter<'offsets, BucketOffset, MAP_BLOCK_SIZE>,

    /// Any offsets that couldn't fit in the offset map due to too many strings within a block.
    pub(super) overflowed_offsets: ThreadLocal<RefCell<Vec<OverflowedOffset>>>,

    pub(super) errors: ArrayQueue<Error>,

    pub(super) reuse_pool: &'scope ReusePool,
}
pub(super) enum StringsSlot<'data, 'offsets> {
    Empty,
    WaitingForStrings(Box<MergeStringsSectionBucket<'data>>),
    Strings(Vec<StringToMerge<'data, 'offsets>>),
}
pub(super) struct ReusePool {
    pub(super) string_vecs: ArrayQueue<Vec<StringToMerge<'static, 'static>>>,

    pub(super) capacity: usize,

    /// Number of Vecs that haven't yet been reserved.
    pub(super) available: AtomicUsize,
}

/// Holds instances of data structures that we reuse where possible. This allows us to reduce the
/// number of separate heap allocations we make.
impl ReusePool {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            string_vecs: ArrayQueue::new(capacity),
            capacity,
            available: AtomicUsize::new(capacity),
        }
    }

    pub(super) fn take_string_merge_vec<'data, 'offsets>(
        &self,
        reservation: &mut PoolReservation,
    ) -> Vec<StringToMerge<'data, 'offsets>> {
        reservation.remaining = reservation.remaining.checked_sub(1).unwrap();
        self.string_vecs
            .pop()
            .map_or_else(|| Vec::with_capacity(1024), reuse_vec)
    }

    pub(super) fn return_strings_to_merge(&self, strings_to_merge: Vec<StringToMerge<'_, '_>>) {
        let r = self.string_vecs.push(reuse_vec(strings_to_merge));
        assert!(r.is_ok());

        self.available.fetch_add(1, Ordering::Relaxed);
    }

    /// Attempt to reserve the specified number of Vecs. Fails if there isn't at least that many
    /// already available.
    pub(super) fn try_reserve(&self, num_vecs: usize) -> Result<PoolReservation, ()> {
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
    pub(super) fn unreserve(&self, reservation: PoolReservation) {
        if reservation.remaining == 0 {
            return;
        }
        self.available
            .fetch_add(reservation.remaining, Ordering::Relaxed);
    }
}

pub(super) struct PoolReservation {
    pub(super) remaining: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BucketOffset(pub(super) u32);

pub(super) struct OverflowedOffset {
    pub(super) input: LinearInputOffset,
    pub(super) output: BucketOffset,
}

impl StringMergeInputSection<'_> {
    /// Returns the length of this section's data rounded up to the next multiple of the block size.
    pub(super) fn padded_len(&self) -> usize {
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
pub(super) fn reuse_vec<T, U>(mut v: Vec<T>) -> Vec<U> {
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
