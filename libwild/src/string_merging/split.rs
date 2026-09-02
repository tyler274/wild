use super::merge::build_merge_class_buckets;
use super::merge::entity_alignment;
use super::merge::merge_bucket_index;
use super::types::*;
use crate::alignment;
use crate::args::Experiment;
use crate::error::Context as _;
use crate::error::Result;
use crate::hash::PreHashed;
use crate::platform;
use crate::verbose_timing_phase;
use crossbeam_queue::ArrayQueue;
use crossbeam_utils::atomic::AtomicCell;
use rayon::Scope;
use sharded_offset_map::OffsetMap;
use sharded_offset_map::ShardedWriter;
use std::mem::replace;
use std::mem::take;
use std::ops::Range;
use std::sync::Mutex;
use thread_local::ThreadLocal;

pub(super) fn process_input_section<'data, 'offsets>(
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
pub(super) fn string_bucket_offset(input: usize, bucket: usize) -> usize {
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
        replace(&mut *lock, slot)
    }
}

// Spawn as many input-processing tasks as allowed.
pub(super) fn try_spawn_input_processing<'scope>(
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

pub(super) fn create_split_resources<'data, 'offsets, 'scope>(
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
pub(super) fn split_sections<'data, 'offsets, 'sections>(
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
pub(super) fn total_input_size(
    input_sections: &[StringMergeInputSection<'_>],
) -> LinearInputOffset {
    input_sections
        .last()
        .map(|sec| {
            sec.start_input_offset
                + (sec.section_data.len() as u64).next_multiple_of(MAP_BLOCK_SIZE)
        })
        .unwrap_or_default()
}

/// Perform initial processing of the input sections in a group.
pub(super) fn process_input_section_group<'data, 'offsets, 'scope>(
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
pub(super) fn work_with_bucket<'data, 'scope>(
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

impl<'data> MergeStringsSectionBucket<'data> {
    pub(super) fn process_split_output(
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
    pub(super) fn add_string(
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

    pub(super) fn new(i: usize) -> Self {
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
