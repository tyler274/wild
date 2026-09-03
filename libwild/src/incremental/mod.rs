//! Incremental linking: initial padded link, then in-place updates with fallbacks.
//!
//! Follows the design in
//! <https://davidlattimore.github.io/posts/2024/11/19/designing-wilds-incremental-linking.html>
//! and issue #184. Dense `SymbolId` / `FileId` values are rebuilt every link. Cross-run identity
//! uses a generational [`AtomTable`]: unchanged files keep their handle, a replaced path reuses a
//! slot with a new generation so stale reverse-reloc nodes cannot alias, and reverse-reloc lists
//! plus resolutions are keyed by `(atom, local symbol)` rather than this-run dense IDs.

mod atoms;

use crate::error::Result;
use crate::hash::hash_bytes;
use crate::input_data::FileId;
use crate::input_data::InputFile;
use crate::platform::Args as _;
use crate::platform::Platform;
pub(crate) use atoms::AtomId;
pub(crate) use atoms::AtomResolutions;
pub(crate) use atoms::AtomTable;
pub(crate) use atoms::ReverseRelocIndex;
pub(crate) use atoms::ReverseRelocNode;
pub(crate) use atoms::read_reverse_relocs;
pub(crate) use atoms::write_reverse_relocs;
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

/// One symbol-bearing input (object, prelude, …) for atom binding and skip planning.
#[derive(Debug, Clone)]
pub(crate) struct IncrementalFileRecord {
    pub(crate) file_id: FileId,
    pub(crate) key: String,
    pub(crate) source_path: PathBuf,
    pub(crate) sizes: Vec<u64>,
    pub(crate) num_symbols: usize,
    pub(crate) skippable: bool,
}

/// Previous resolutions and reverse-reloc index, used to patch skipped objects.
#[derive(Debug)]
pub(crate) struct IncrementalPatchJob {
    pub(crate) old_resolutions: AtomResolutions,
    pub(crate) reverse_relocs: ReverseRelocIndex,
}

#[derive(Debug)]
pub(crate) struct IncrementalSession {
    pub(crate) mode: IncrementalMode,
    pub(crate) state_dir: PathBuf,
    pub(crate) fallback_reason: Option<String>,
    skip_payload_count: usize,
    pub(crate) atoms: AtomTable,
    /// FileIds whose atom was reused from the previous run (same key and symbol count).
    reused_files: HashSet<FileId>,
    pub(crate) previous_resolutions: Option<AtomResolutions>,
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
        let atoms = if mode == IncrementalMode::Update {
            AtomTable::read(&state_dir.join("atoms.txt")).unwrap_or_default()
        } else {
            AtomTable::default()
        };
        let (previous_resolutions, previous_reverse_relocs) = if mode == IncrementalMode::Update {
            (
                AtomResolutions::read(&state_dir.join("resolutions.bin")),
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
            atoms,
            reused_files: HashSet::new(),
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

    /// Assign generational atoms to this run's files. Reuses handles for unchanged keys.
    pub(crate) fn bind_files(
        &mut self,
        records: &[IncrementalFileRecord],
    ) -> HashMap<FileId, AtomId> {
        let mut map = HashMap::new();
        self.reused_files.clear();
        let live_keys: HashSet<&str> = records.iter().map(|r| r.key.as_str()).collect();

        let stale: Vec<AtomId> = self
            .atoms
            .live_ids()
            .filter(|id| {
                self.atoms
                    .key(*id)
                    .is_none_or(|key| !live_keys.contains(key))
            })
            .collect();
        if !stale.is_empty() && self.mode == IncrementalMode::Update {
            // Removing a contributing object usually changes layout; `plan_in_place_update`
            // still runs the layout/size checks. Free the slots so generation bumps.
        }
        for id in stale {
            self.atoms.free(id);
        }

        for rec in records {
            let num_symbols = u32::try_from(rec.num_symbols).unwrap_or(u32::MAX);
            if let Some(id) = self.atoms.get_by_key(&rec.key) {
                if self.atoms.num_symbols(id) != Some(num_symbols) {
                    self.record_fallback("symbol count changed");
                    self.atoms.free(id);
                    let new_id = self.atoms.alloc(rec.key.clone(), num_symbols);
                    map.insert(rec.file_id, new_id);
                } else {
                    self.reused_files.insert(rec.file_id);
                    map.insert(rec.file_id, id);
                }
            } else {
                if self.mode == IncrementalMode::Update && rec.skippable {
                    // New object: cannot skip its payload. Layout/size checks decide fallback.
                }
                let id = self.atoms.alloc(rec.key.clone(), num_symbols);
                map.insert(rec.file_id, id);
            }
        }
        map
    }

    /// Decide which objects can keep their existing section payloads. Returns a fallback reason
    /// when the update cannot be applied in place.
    pub(crate) fn plan_in_place_update<D: crate::InputFileData>(
        &mut self,
        sections: &[PersistedSection],
        records: &[IncrementalFileRecord],
        loaded_files: &[&InputFile<D>],
    ) -> HashSet<FileId> {
        if self.mode != IncrementalMode::Update || self.fallback_reason.is_some() {
            return HashSet::new();
        }
        match plan_skip_payloads(
            &self.state_dir,
            sections,
            records,
            loaded_files,
            &self.reused_files,
        ) {
            Ok(skip) => {
                if !skip.is_empty()
                    && (self.previous_resolutions.is_none()
                        || self.previous_reverse_relocs.is_none())
                {
                    self.record_fallback("missing incremental graph");
                    return HashSet::new();
                }
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
        resolutions: &AtomResolutions,
        reverse_relocs: &ReverseRelocIndex,
        records: &[IncrementalFileRecord],
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
        for file in loaded_files {
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
            snapshot_input(
                &file.filename,
                &input_copy_path(&self.state_dir, &file.filename),
            )?;
        }

        let mut sections_file = fs::File::create(self.state_dir.join("sections.txt"))?;
        for section in sections {
            writeln!(
                sections_file,
                "{} {} {} {}",
                section.file_offset, section.file_size, section.mem_size, section.name
            )?;
        }

        self.atoms.write(&self.state_dir.join("atoms.txt"))?;
        resolutions.write(&self.state_dir.join("resolutions.bin"))?;

        // A skip update only rewrites changed objects, so the in-memory index is incomplete.
        // Layout is unchanged, so previously recorded sites remain valid under atom keys.
        if self.skip_payload_count == 0 || !self.state_dir.join("reverse_relocs.bin").is_file() {
            write_reverse_relocs(&self.state_dir.join("reverse_relocs.bin"), reverse_relocs)?;
        }

        let mut sizes_file = fs::File::create(self.state_dir.join("object_sizes.txt"))?;
        for rec in records {
            if !rec.skippable {
                continue;
            }
            let sizes = rec
                .sizes
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(",");
            writeln!(sizes_file, "{sizes}\t{}", rec.key)?;
        }

        Ok(())
    }
}

pub(crate) fn incremental_state_dir(output: &Path) -> PathBuf {
    let mut dir = output.as_os_str().to_os_string();
    dir.push(".incr");
    PathBuf::from(dir)
}

fn input_copy_path(state_dir: &Path, filename: &Path) -> PathBuf {
    let hash = hash_bytes(filename.as_os_str().as_encoded_bytes());
    state_dir.join("copies").join(format!("{hash:016x}"))
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

fn load_object_sizes(state_dir: &Path) -> Option<HashMap<String, Vec<u64>>> {
    let text = fs::read_to_string(state_dir.join("object_sizes.txt")).ok()?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let (sizes, key) = line.split_once('\t')?;
        let sizes = if sizes.is_empty() {
            Vec::new()
        } else {
            sizes
                .split(',')
                .map(|s| s.parse().ok())
                .collect::<Option<Vec<u64>>>()?
        };
        map.insert(key.to_owned(), sizes);
    }
    Some(map)
}

fn plan_skip_payloads<D: crate::InputFileData>(
    state_dir: &Path,
    sections: &[PersistedSection],
    records: &[IncrementalFileRecord],
    loaded_files: &[&InputFile<D>],
    reused_files: &HashSet<FileId>,
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
    for rec in records {
        if !rec.skippable {
            continue;
        }
        match previous_sizes.get(&rec.key) {
            Some(prev) if prev == &rec.sizes => {}
            Some(_) => return Err("input section size changed".to_owned()),
            None => return Err("new input object".to_owned()),
        }
    }

    let diff = diff_input_paths(state_dir, loaded_files);
    let mut skip = HashSet::new();
    for rec in records {
        if rec.skippable
            && reused_files.contains(&rec.file_id)
            && !diff.changed.contains(&rec.source_path)
        {
            skip.insert(rec.file_id);
        }
    }
    Ok(skip)
}

struct InputDiff {
    changed: HashSet<PathBuf>,
}

fn diff_input_paths<D: crate::InputFileData>(
    state_dir: &Path,
    loaded_files: &[&InputFile<D>],
) -> InputDiff {
    let Ok(previous) = fs::read_to_string(state_dir.join("inputs.txt")) else {
        return InputDiff {
            changed: loaded_files.iter().map(|f| f.filename.clone()).collect(),
        };
    };
    let mut previous_by_path: HashMap<String, String> = HashMap::new();
    for prev in previous.lines().filter(|l| !l.is_empty()) {
        let Some((_, prev_path)) = prev.rsplit_once(' ') else {
            continue;
        };
        previous_by_path.insert(prev_path.to_owned(), prev.to_owned());
    }

    let mut changed = HashSet::new();
    for file in loaded_files {
        let path_key = file.filename.as_os_str().to_string_lossy();
        let Some(prev) = previous_by_path.get(path_key.as_ref()) else {
            changed.insert(file.filename.clone());
            continue;
        };
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
        let identity_changed = current.trim() != prev.as_str();
        let bytes_changed = {
            let copy = input_copy_path(state_dir, &file.filename);
            match (fs::read(&copy), fs::read(&file.filename)) {
                (Ok(old), Ok(new)) => old != new,
                _ => identity_changed,
            }
        };
        if identity_changed || bytes_changed {
            changed.insert(file.filename.clone());
        }
    }
    InputDiff { changed }
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
    fn bind_files_reuses_atoms_across_reorder() {
        let mut session = IncrementalSession {
            mode: IncrementalMode::Initial,
            state_dir: PathBuf::from("/tmp"),
            fallback_reason: None,
            skip_payload_count: 0,
            atoms: AtomTable::default(),
            reused_files: HashSet::new(),
            previous_resolutions: None,
            previous_reverse_relocs: None,
        };
        let a = IncrementalFileRecord {
            file_id: FileId::new(1, 0),
            key: "a.o".into(),
            source_path: PathBuf::from("a.o"),
            sizes: vec![4],
            num_symbols: 2,
            skippable: true,
        };
        let b = IncrementalFileRecord {
            file_id: FileId::new(1, 1),
            key: "b.o".into(),
            source_path: PathBuf::from("b.o"),
            sizes: vec![8],
            num_symbols: 3,
            skippable: true,
        };
        let first = session.bind_files(&[a.clone(), b.clone()]);
        session.mode = IncrementalMode::Update;
        let a2 = IncrementalFileRecord {
            file_id: FileId::new(2, 0),
            ..b
        };
        let b2 = IncrementalFileRecord {
            file_id: FileId::new(2, 1),
            ..a
        };
        let second = session.bind_files(&[a2.clone(), b2.clone()]);
        assert_eq!(first[&FileId::new(1, 0)], second[&FileId::new(2, 1)]);
        assert_eq!(first[&FileId::new(1, 1)], second[&FileId::new(2, 0)]);
        assert!(session.reused_files.contains(&FileId::new(2, 0)));
        assert!(session.reused_files.contains(&FileId::new(2, 1)));
        assert!(session.fallback_reason.is_none());
    }

    #[test]
    fn bind_files_bumps_generation_when_key_replaced() {
        let mut session = IncrementalSession {
            mode: IncrementalMode::Initial,
            state_dir: PathBuf::from("/tmp"),
            fallback_reason: None,
            skip_payload_count: 0,
            atoms: AtomTable::default(),
            reused_files: HashSet::new(),
            previous_resolutions: None,
            previous_reverse_relocs: None,
        };
        let foo = IncrementalFileRecord {
            file_id: FileId::new(1, 0),
            key: "foo.o".into(),
            source_path: PathBuf::from("foo.o"),
            sizes: vec![4],
            num_symbols: 1,
            skippable: true,
        };
        let first = session.bind_files(&[foo]);
        let old = first[&FileId::new(1, 0)];
        session.mode = IncrementalMode::Update;
        let bar = IncrementalFileRecord {
            file_id: FileId::new(1, 0),
            key: "bar.o".into(),
            source_path: PathBuf::from("bar.o"),
            sizes: vec![4],
            num_symbols: 1,
            skippable: true,
        };
        let second = session.bind_files(&[bar]);
        let new = second[&FileId::new(1, 0)];
        assert_eq!(old.index, new.index);
        assert_ne!(old.generation, new.generation);
        assert!(!session.atoms.get(old));
        assert!(session.atoms.get(new));
        assert!(!session.reused_files.contains(&FileId::new(1, 0)));
    }

    #[test]
    fn copy_path_is_stable_for_filename() {
        let dir = Path::new("/tmp/out.incr");
        let a = input_copy_path(dir, Path::new("/work/a.o"));
        let b = input_copy_path(dir, Path::new("/work/a.o"));
        let c = input_copy_path(dir, Path::new("/work/b.o"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
