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

pub(crate) mod merge;
pub(crate) mod split;
pub(crate) mod types;

#[allow(unused_imports)]
pub(crate) use merge::*;
#[allow(unused_imports)]
pub(crate) use split::*;
#[allow(unused_imports)]
pub(crate) use types::*;
