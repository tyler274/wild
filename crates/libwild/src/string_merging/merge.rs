use super::split::create_split_resources;
use super::split::try_spawn_input_processing;
use super::types::*;
use crate::alignment;
use crate::args::Experiment;
use crate::bail;
use crate::error::Result;
use crate::input_section_id::SectionIdRange;
use crate::layout::EnginePlatform;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Symbol as _;
use crate::resolution::ResolvedFile;
use crate::resolution::ResolvedGroup;
use crate::resolution::SectionSlot;
use crate::timing_phase;
use crate::verbose_timing_phase;
use hashbrown::HashMap;
use itertools::Itertools as _;
use std::sync::atomic::Ordering;

pub(super) fn build_merge_class_buckets(
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

pub(super) fn merge_bucket_index(
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
pub(super) fn pad_merge_buckets(
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

/// BFD `record_section` entity alignment: the largest power of two that
/// divides `offset_in_section`, capped at the input section's `sh_addralign`.
pub(super) fn entity_alignment(
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
pub(crate) fn merge_strings<'data, P: EnginePlatform>(
    inputs: &StringMergeInputs<'data>,
    output_sections: &OutputSections<P>,
    args: &P::Args,
) -> Result<OutputSectionMap<MergedStringsSection<'data>>> {
    timing_phase!("Merge strings");

    let mut output_string_sections = output_sections.new_section_map::<MergedStringsSection>();

    let num_threads = rayon::current_num_threads();
    let split_parallelism = args.numeric_experiment(
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
    pub(crate) fn new<P: EnginePlatform>(
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
fn group_merge_string_sections_by_output<'data, P: EnginePlatform>(
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
impl<'data> MergedStringsSection<'data> {
    pub(super) fn add_input_sections(
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
impl BucketOffset {
    pub(super) fn new(offset: u32, bucket: usize) -> Result<Self> {
        if offset >= 1 << (32 - MERGE_STRING_BUCKET_BITS) {
            bail!("Merge-string bucket too large");
        }
        Ok(BucketOffset(
            ((bucket as u32) << (32 - MERGE_STRING_BUCKET_BITS)) | offset,
        ))
    }

    pub(super) fn bucket(self) -> usize {
        (self.0 >> (32 - MERGE_STRING_BUCKET_BITS)) as usize
    }

    pub(super) fn offset_in_bucket(self) -> u64 {
        u64::from(self.0 & ((1 << (32 - MERGE_STRING_BUCKET_BITS)) - 1))
    }
}
/// Looks for a merged string at `symbol_index` + `addend` in the input and if found, returns its
/// address in the output.
#[inline(always)]
pub(crate) fn get_merged_string_output_address<'data, P: EnginePlatform>(
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

pub(super) fn remap_tail_offset(
    offset: BucketOffset,
    section: &MergedStringsSection<'_>,
) -> BucketOffset {
    section.tail_remap.get(&offset.0).copied().unwrap_or(offset)
}

pub(super) fn find_string(
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
    pub(crate) fn compute<P: EnginePlatform>(
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
