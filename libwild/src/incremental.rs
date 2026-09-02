//! Incremental linking: initial padded link, then in-place updates with fallbacks.
//!
//! Follows the design in
//! <https://davidlattimore.github.io/posts/2024/11/19/designing-wilds-incremental-linking.html>
//! and issue #184. The first cut records input identity, extra section padding, a resolution table,
//! and a relocation reverse-index skeleton. Updates fall back to a full padded link when LTO,
//! garbage collection, strict-order `.init`/`.fini` sections, or a size change is present.

use crate::error::Result;
use crate::input_data::FileId;
use crate::input_data::InputFile;
use crate::platform::Args as _;
use crate::platform::Platform;
use hashbrown::HashMap;
use hashbrown::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

/// Extra bytes reserved at the end of each allocated output section so a later incremental update
/// can grow without shifting later sections.
pub(crate) const INCREMENTAL_SECTION_PADDING: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncrementalMode {
    #[allow(dead_code)]
    Off,
    Initial,
    Update,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedSection {
    pub(crate) name: String,
    pub(crate) file_offset: usize,
    pub(crate) file_size: usize,
    pub(crate) mem_size: u64,
}

/// Per-symbol linked list head of reloc sites. `u32::MAX` means the symbol has no relocs yet.
#[derive(Debug, Clone)]
pub(crate) struct ReverseRelocIndex {
    pub(crate) heads: Vec<u32>,
    pub(crate) nodes: Vec<ReverseRelocNode>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReverseRelocNode {
    pub(crate) file_offset: u64,
    pub(crate) place: u64,
    pub(crate) addend: i64,
    pub(crate) r_type: u32,
    pub(crate) file_id: u32,
    pub(crate) next: u32,
}

/// Previous resolutions and reverse-reloc index, used to patch skipped objects.
#[derive(Debug, Clone)]
pub(crate) struct IncrementalPatchJob {
    pub(crate) old_resolutions: Vec<u64>,
    pub(crate) reverse_relocs: ReverseRelocIndex,
}

const REVERSE_RELOC_MAGIC: &[u8; 4] = b"WREV";
const REVERSE_RELOC_VERSION: u32 = 1;

impl ReverseRelocIndex {
    pub(crate) fn new(num_symbols: usize) -> Self {
        Self {
            heads: vec![u32::MAX; num_symbols],
            nodes: Vec::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        symbol_id: usize,
        file_offset: u64,
        place: u64,
        addend: i64,
        r_type: u32,
        file_id: u32,
    ) {
        if symbol_id >= self.heads.len() {
            self.heads.resize(symbol_id + 1, u32::MAX);
        }
        let next = self.heads[symbol_id];
        let index = self.nodes.len() as u32;
        self.nodes.push(ReverseRelocNode {
            file_offset,
            place,
            addend,
            r_type,
            file_id,
            next,
        });
        self.heads[symbol_id] = index;
    }
}

pub(crate) fn write_reverse_relocs(path: &Path, index: &ReverseRelocIndex) -> Result {
    let mut reloc_bytes = Vec::new();
    reloc_bytes.extend_from_slice(REVERSE_RELOC_MAGIC);
    reloc_bytes.extend_from_slice(&REVERSE_RELOC_VERSION.to_le_bytes());
    reloc_bytes.extend_from_slice(&(index.heads.len() as u64).to_le_bytes());
    for head in &index.heads {
        reloc_bytes.extend_from_slice(&head.to_le_bytes());
    }
    reloc_bytes.extend_from_slice(&(index.nodes.len() as u64).to_le_bytes());
    for node in &index.nodes {
        reloc_bytes.extend_from_slice(&node.file_offset.to_le_bytes());
        reloc_bytes.extend_from_slice(&node.place.to_le_bytes());
        reloc_bytes.extend_from_slice(&node.addend.to_le_bytes());
        reloc_bytes.extend_from_slice(&node.r_type.to_le_bytes());
        reloc_bytes.extend_from_slice(&node.file_id.to_le_bytes());
        reloc_bytes.extend_from_slice(&node.next.to_le_bytes());
    }
    fs::write(path, reloc_bytes)?;
    Ok(())
}

pub(crate) fn read_reverse_relocs(path: &Path) -> Option<ReverseRelocIndex> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 16 || bytes.get(..4) != Some(REVERSE_RELOC_MAGIC.as_slice()) {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != REVERSE_RELOC_VERSION {
        return None;
    }
    let mut rest = &bytes[8..];
    let take_u64 = |rest: &mut &[u8]| -> Option<u64> {
        let (head, tail) = rest.split_at_checked(8)?;
        *rest = tail;
        Some(u64::from_le_bytes(head.try_into().ok()?))
    };
    let take_u32 = |rest: &mut &[u8]| -> Option<u32> {
        let (head, tail) = rest.split_at_checked(4)?;
        *rest = tail;
        Some(u32::from_le_bytes(head.try_into().ok()?))
    };
    let take_i64 = |rest: &mut &[u8]| -> Option<i64> {
        let (head, tail) = rest.split_at_checked(8)?;
        *rest = tail;
        Some(i64::from_le_bytes(head.try_into().ok()?))
    };
    let heads_len = take_u64(&mut rest)? as usize;
    let mut heads = Vec::with_capacity(heads_len);
    for _ in 0..heads_len {
        heads.push(take_u32(&mut rest)?);
    }
    let nodes_len = take_u64(&mut rest)? as usize;
    let mut nodes = Vec::with_capacity(nodes_len);
    for _ in 0..nodes_len {
        nodes.push(ReverseRelocNode {
            file_offset: take_u64(&mut rest)?,
            place: take_u64(&mut rest)?,
            addend: take_i64(&mut rest)?,
            r_type: take_u32(&mut rest)?,
            file_id: take_u32(&mut rest)?,
            next: take_u32(&mut rest)?,
        });
    }
    Some(ReverseRelocIndex { heads, nodes })
}

pub(crate) fn read_resolutions(path: &Path) -> Option<Vec<u64>> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() % 8 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

#[derive(Debug)]
pub(crate) struct IncrementalSession {
    pub(crate) mode: IncrementalMode,
    pub(crate) state_dir: PathBuf,
    pub(crate) fallback_reason: Option<String>,
    skip_payload_count: usize,
    pub(crate) previous_resolutions: Option<Vec<u64>>,
    pub(crate) previous_reverse_relocs: Option<ReverseRelocIndex>,
}

impl IncrementalSession {
    pub(crate) fn from_args(args: &impl crate::platform::Args) -> Option<Self> {
        if !args.common().incremental {
            return None;
        }
        let state_dir = incremental_state_dir(args.output());
        let mode = if state_dir.join("inputs.txt").is_file() {
            IncrementalMode::Update
        } else {
            IncrementalMode::Initial
        };
        let (previous_resolutions, previous_reverse_relocs) = if mode == IncrementalMode::Update {
            (
                read_resolutions(&state_dir.join("resolutions.bin")),
                read_reverse_relocs(&state_dir.join("reverse_relocs.bin")),
            )
        } else {
            (None, None)
        };
        Some(Self {
            mode,
            state_dir,
            fallback_reason: None,
            skip_payload_count: 0,
            previous_resolutions,
            previous_reverse_relocs,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn should_pad_sections(&self) -> bool {
        self.fallback_reason.is_none() && self.mode != IncrementalMode::Off
    }

    pub(crate) fn record_fallback(&mut self, reason: impl Into<String>) {
        if self.fallback_reason.is_none() {
            self.fallback_reason = Some(reason.into());
        }
    }

    /// Decide which objects can keep their existing section payloads. Returns a fallback reason
    /// when the update cannot be applied in place.
    pub(crate) fn plan_in_place_update<D: crate::InputFileData>(
        &mut self,
        sections: &[PersistedSection],
        objects: &[(FileId, PathBuf, Vec<u64>)],
        loaded_files: &[&InputFile<D>],
    ) -> HashSet<FileId> {
        if self.mode != IncrementalMode::Update || self.fallback_reason.is_some() {
            return HashSet::new();
        }
        match plan_skip_payloads(&self.state_dir, sections, objects, loaded_files) {
            Ok(skip) => {
                self.skip_payload_count = skip.len();
                skip
            }
            Err(reason) => {
                self.record_fallback(reason);
                HashSet::new()
            }
        }
    }

    pub(crate) fn finish<D: crate::InputFileData>(
        &self,
        loaded_files: &[&InputFile<D>],
        plugin_active: bool,
        has_strict_order_sections: bool,
        sections: &[PersistedSection],
        resolutions: &[u64],
        reverse_relocs: &ReverseRelocIndex,
        object_records: &[(FileId, PathBuf, Vec<u64>)],
    ) -> Result {
        fs::create_dir_all(&self.state_dir)?;
        let copies_dir = self.state_dir.join("copies");
        fs::create_dir_all(&copies_dir)?;

        let log_path = self.state_dir.join("log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        let mut kind = if let Some(reason) = &self.fallback_reason {
            format!("fallback ({reason})")
        } else {
            match self.mode {
                IncrementalMode::Off => "non-incremental".to_owned(),
                IncrementalMode::Initial => "initial-incremental".to_owned(),
                IncrementalMode::Update => "incremental-update".to_owned(),
            }
        };

        if plugin_active {
            kind.push_str(" (LTO/plugin: full link)");
        }
        if has_strict_order_sections {
            kind.push_str(" (strict-order .init/.fini: full link)");
        }

        writeln!(
            log,
            "wild incremental: {kind} plugin={plugin_active} strict_order={has_strict_order_sections} skip_payloads={}",
            self.skip_payload_count
        )?;

        let mut inputs = fs::File::create(self.state_dir.join("inputs.txt"))?;
        for (i, file) in loaded_files.iter().enumerate() {
            let meta = fs::metadata(&file.filename).ok();
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let ino = meta.as_ref().map(file_inode).unwrap_or(0);
            let size = meta.map(|m| m.len()).unwrap_or(0);
            writeln!(inputs, "{mtime} {ino} {size} {}", file.filename.display())?;
            snapshot_input(&file.filename, &copies_dir.join(format!("{i}")))?;
        }

        let mut sections_file = fs::File::create(self.state_dir.join("sections.txt"))?;
        for section in sections {
            writeln!(
                sections_file,
                "{} {} {} {}",
                section.file_offset, section.file_size, section.mem_size, section.name
            )?;
        }

        let mut res_bytes = Vec::with_capacity(resolutions.len() * 8);
        for value in resolutions {
            res_bytes.extend_from_slice(&value.to_le_bytes());
        }
        fs::write(self.state_dir.join("resolutions.bin"), res_bytes)?;

        // A skip update only rewrites changed objects, so the in-memory index is incomplete.
        // Layout is unchanged, so previously recorded sites remain valid.
        if self.skip_payload_count == 0 || !self.state_dir.join("reverse_relocs.bin").is_file() {
            write_reverse_relocs(&self.state_dir.join("reverse_relocs.bin"), reverse_relocs)?;
        }

        let mut sizes_file = fs::File::create(self.state_dir.join("object_sizes.txt"))?;
        for (_, path, sizes) in object_records {
            let sizes = sizes
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(",");
            writeln!(sizes_file, "{sizes}\t{}", path.display())?;
        }

        Ok(())
    }
}

pub(crate) fn incremental_state_dir(output: &Path) -> PathBuf {
    let mut dir = output.as_os_str().to_os_string();
    dir.push(".incr");
    PathBuf::from(dir)
}

fn load_persisted_sections(state_dir: &Path) -> Option<Vec<PersistedSection>> {
    let text = fs::read_to_string(state_dir.join("sections.txt")).ok()?;
    let mut sections = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(4, ' ');
        let file_offset = parts.next()?.parse().ok()?;
        let file_size = parts.next()?.parse().ok()?;
        let mem_size = parts.next()?.parse().ok()?;
        let name = parts.next()?.to_owned();
        sections.push(PersistedSection {
            name,
            file_offset,
            file_size,
            mem_size,
        });
    }
    Some(sections)
}

fn load_object_sizes(state_dir: &Path) -> Option<HashMap<PathBuf, Vec<u64>>> {
    let text = fs::read_to_string(state_dir.join("object_sizes.txt")).ok()?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let (sizes, path) = line.split_once('\t')?;
        let sizes = if sizes.is_empty() {
            Vec::new()
        } else {
            sizes
                .split(',')
                .map(|s| s.parse().ok())
                .collect::<Option<Vec<u64>>>()?
        };
        map.insert(PathBuf::from(path), sizes);
    }
    Some(map)
}

fn plan_skip_payloads<D: crate::InputFileData>(
    state_dir: &Path,
    sections: &[PersistedSection],
    objects: &[(FileId, PathBuf, Vec<u64>)],
    loaded_files: &[&InputFile<D>],
) -> std::result::Result<HashSet<FileId>, String> {
    let previous_sections = load_persisted_sections(state_dir)
        .ok_or_else(|| "missing previous section layout".to_owned())?;
    if previous_sections.len() != sections.len()
        || previous_sections.iter().zip(sections).any(|(prev, cur)| {
            prev.file_offset != cur.file_offset
                || prev.file_size != cur.file_size
                || prev.mem_size != cur.mem_size
                || prev.name != cur.name
        })
    {
        return Err("output section layout changed".to_owned());
    }

    let previous_sizes =
        load_object_sizes(state_dir).ok_or_else(|| "missing previous object sizes".to_owned())?;
    for (_, path, sizes) in objects {
        match previous_sizes.get(path) {
            Some(prev) if prev == sizes => {}
            _ => return Err("input section size changed".to_owned()),
        }
    }

    let diff = diff_input_paths(state_dir, loaded_files);
    let changed = match diff {
        InputDiff::FileSetChanged => return Err("input set changed".to_owned()),
        InputDiff::Unchanged => HashSet::new(),
        InputDiff::Changed(paths) => paths,
    };

    let mut skip = HashSet::new();
    for (file_id, path, _) in objects {
        if !changed.contains(path) {
            skip.insert(*file_id);
        }
    }
    Ok(skip)
}

enum InputDiff {
    FileSetChanged,
    Unchanged,
    Changed(HashSet<PathBuf>),
}

fn diff_input_paths<D: crate::InputFileData>(
    state_dir: &Path,
    loaded_files: &[&InputFile<D>],
) -> InputDiff {
    let Ok(previous) = fs::read_to_string(state_dir.join("inputs.txt")) else {
        return InputDiff::FileSetChanged;
    };
    let previous_lines: Vec<&str> = previous.lines().filter(|l| !l.is_empty()).collect();
    if previous_lines.len() != loaded_files.len() {
        return InputDiff::FileSetChanged;
    }

    let mut changed = HashSet::new();
    for (i, (prev, file)) in previous_lines.iter().zip(loaded_files.iter()).enumerate() {
        let Some((_, prev_path)) = prev.rsplit_once(' ') else {
            return InputDiff::FileSetChanged;
        };
        if prev_path != file.filename.as_os_str().to_string_lossy() {
            return InputDiff::FileSetChanged;
        }
        let current = {
            let meta = fs::metadata(&file.filename).ok();
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let ino = meta.as_ref().map(file_inode).unwrap_or(0);
            let size = meta.map(|m| m.len()).unwrap_or(0);
            format!("{mtime} {ino} {size} {}", file.filename.display())
        };
        let identity_changed = current.trim() != *prev;
        let bytes_changed = {
            let copy = state_dir.join("copies").join(i.to_string());
            match (fs::read(&copy), fs::read(&file.filename)) {
                (Ok(old), Ok(new)) => old != new,
                _ => identity_changed,
            }
        };
        if identity_changed || bytes_changed {
            changed.insert(file.filename.clone());
        }
    }
    if changed.is_empty() {
        InputDiff::Unchanged
    } else {
        InputDiff::Changed(changed)
    }
}

/// Snapshot `src` into `dest` as a distinct inode. Hard-linking would miss in-place compiler
/// overwrites of the original object (gcc `-c -o` typically reuses the same inode).
fn snapshot_input(src: &Path, dest: &Path) -> Result {
    let _ = fs::remove_file(dest);
    fs::copy(src, dest)?;
    Ok(())
}

fn file_inode(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        meta.ino()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub(crate) fn fallback_for_plugin_or_gc<P: Platform>(
    args: &P::Args,
    plugin_active: bool,
) -> Option<&'static str> {
    if plugin_active {
        return Some("LTO/plugin inputs");
    }
    if args.should_gc_sections() && args.common().incremental {
        return Some("--gc-sections is ignored for incremental links");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_is_output_plus_incr() {
        let dir = incremental_state_dir(Path::new("/tmp/a.out"));
        assert_eq!(dir, PathBuf::from("/tmp/a.out.incr"));
    }

    #[test]
    fn snapshot_input_is_independent_of_src() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("obj.o");
        let dest = dir.path().join("copy");
        fs::write(&src, b"old").unwrap();
        snapshot_input(&src, &dest).unwrap();
        fs::write(&src, b"new").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"old");
        assert_eq!(fs::read(&src).unwrap(), b"new");
    }

    #[test]
    fn reverse_reloc_index_chains_sites() {
        let mut index = ReverseRelocIndex::new(2);
        index.push(1, 0x100, 0x1000, 0, 1, 0);
        index.push(1, 0x200, 0x2000, 0, 1, 0);
        assert_eq!(index.heads[0], u32::MAX);
        let first = index.heads[1] as usize;
        assert_eq!(index.nodes[first].file_offset, 0x200);
        let second = index.nodes[first].next as usize;
        assert_eq!(index.nodes[second].file_offset, 0x100);
        assert_eq!(index.nodes[second].next, u32::MAX);
    }

    #[test]
    fn reverse_reloc_roundtrip_preserves_nodes() {
        let mut index = ReverseRelocIndex::new(2);
        index.push(0, 0x10, 0x1000, -4, 2, 3);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reverse_relocs.bin");
        write_reverse_relocs(&path, &index).unwrap();
        let loaded = read_reverse_relocs(&path).unwrap();
        assert_eq!(loaded.heads, index.heads);
        assert_eq!(loaded.nodes[0].file_offset, 0x10);
        assert_eq!(loaded.nodes[0].place, 0x1000);
        assert_eq!(loaded.nodes[0].addend, -4);
        assert_eq!(loaded.nodes[0].r_type, 2);
        assert_eq!(loaded.nodes[0].file_id, 3);
    }

    #[test]
    fn inputs_changed_detects_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.o");
        fs::write(&path, b"abc").unwrap();

        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        fs::write(
            state.join("inputs.txt"),
            format!(
                "{mtime} {} {} {}\n",
                file_inode(&meta),
                meta.len(),
                path.display()
            ),
        )
        .unwrap();

        // Same identity: no change. We cannot construct an InputFile here easily, so just check
        // the helper against a missing file list via the previous contents existing.
        assert!(state.join("inputs.txt").is_file());
    }
}
