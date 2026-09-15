/// Folder snapshots and change diffs.
///
/// A snapshot records one metadata observation per file — size, mtime and, when
/// the platform supplies complete evidence, a digest over the full-resolution
/// stamp (nanosecond mtime, birth time, and on unix ctime and inode). Diffing
/// two observations is then a pure comparison; no file is read unless the caller
/// asks for content verification.
use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};
use rayon::prelude::*;
use serde_json::{Value, json};
use tgrep_core::walker::{self, MetaWalkOptions};

use crate::mcp::query::ToolOutput;
use crate::mcp::state::McpState;

const MAGIC: &[u8; 8] = b"TGSNAP\x01\x00";
const FLAG_CONTENT_HASHES: u32 = 1;
const SNAPSHOT_EXT: &str = "tgsnap";
const DEFAULT_MAX_ENTRIES: usize = 200;
const MAX_MAX_ENTRIES: usize = 5000;
/// Directories reported in the aggregate breakdown of a large diff.
const TOP_DIRECTORIES: usize = 10;

#[derive(Clone)]
struct Entry {
    size: u64,
    mtime: u64,
    /// Digest over every change-detection field the platform supplied. `None`
    /// on platforms or files where that evidence is incomplete, which falls the
    /// comparison back to size and whole-second mtime.
    digest: Option<[u8; 16]>,
    hash: Option<[u8; 32]>,
}

struct Snapshot {
    id: String,
    label: String,
    created_at: u64,
    root: String,
    has_hashes: bool,
    files: BTreeMap<String, Entry>,
}

impl Snapshot {
    fn describe(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "created_at": self.created_at,
            "age": age(self.created_at),
            "files": self.files.len(),
            "content_hashes": self.has_hashes,
            "root": self.root,
        })
    }
}

fn snapshots_dir(state: &McpState) -> PathBuf {
    state.index_dir.join("snapshots")
}

fn snapshot_path(state: &McpState, id: &str) -> PathBuf {
    snapshots_dir(state).join(format!("{id}.{SNAPSHOT_EXT}"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn age(created_at: u64) -> String {
    let seconds = now().saturating_sub(created_at);
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

/// Reject anything that would not round-trip as a plain file name.
fn sanitize_id(label: &str) -> Result<String> {
    let id: String = label.trim().to_string();
    if id.is_empty() || id.len() > 64 {
        bail!("`label` must be 1-64 characters");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("`label` may only contain letters, digits, '.', '_' and '-'");
    }
    Ok(id)
}

fn walk_options(state: &McpState, no_ignore: bool) -> MetaWalkOptions {
    MetaWalkOptions {
        exclude_dirs: state.opts.exclude_dirs.clone(),
        exclude_paths: vec![state.index_dir.clone()],
        no_ignore: no_ignore || state.opts.no_ignore,
        no_require_git: state.opts.no_require_git,
        // A snapshot describes the folder, not the searchable corpus.
        max_file_size: None,
    }
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

/// Observe the tree right now.
fn observe(state: &McpState, no_ignore: bool, with_hashes: bool) -> Result<Snapshot> {
    let walk = walker::walk_file_metadata(&state.root, &walk_options(state, no_ignore));
    let mut files = BTreeMap::new();
    for file in walk.files {
        files.insert(
            normalize(&file.relative_path),
            Entry {
                size: file.size,
                mtime: file.mtime,
                digest: file.version.as_ref().and_then(|v| v.evidence_digest()),
                hash: None,
            },
        );
    }
    if with_hashes {
        let hashes: Vec<(String, Option<[u8; 32]>)> = files
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|path| {
                let hash = hash_file(&state.root.join(&path)).ok();
                (path, hash)
            })
            .collect();
        for (path, hash) in hashes {
            if let Some(entry) = files.get_mut(&path) {
                entry.hash = hash;
            }
        }
    }
    Ok(Snapshot {
        id: String::new(),
        label: String::new(),
        created_at: now(),
        root: crate::search::display_path(&state.root),
        has_hashes: with_hashes,
        files,
    })
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(*hasher.finalize().as_bytes());
        }
        hasher.update(&buffer[..read]);
    }
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    out.write_all(MAGIC)?;
    out.write_all(&snapshot.created_at.to_le_bytes())?;
    let flags = if snapshot.has_hashes {
        FLAG_CONTENT_HASHES
    } else {
        0
    };
    out.write_all(&flags.to_le_bytes())?;
    write_string(&mut out, &snapshot.root)?;
    write_string(&mut out, &snapshot.label)?;
    out.write_all(&(snapshot.files.len() as u64).to_le_bytes())?;
    for (path, entry) in &snapshot.files {
        write_string(&mut out, path)?;
        out.write_all(&entry.size.to_le_bytes())?;
        out.write_all(&entry.mtime.to_le_bytes())?;
        match entry.digest {
            Some(digest) => {
                out.write_all(&[1])?;
                out.write_all(&digest)?;
            }
            None => out.write_all(&[0])?,
        }
        match entry.hash {
            Some(hash) => {
                out.write_all(&[1])?;
                out.write_all(&hash)?;
            }
            None => out.write_all(&[0])?,
        }
    }
    out.flush()?;
    Ok(())
}

fn write_string(out: &mut impl Write, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len()).map_err(|_| anyhow::anyhow!("string too long"))?;
    out.write_all(&len.to_le_bytes())?;
    out.write_all(bytes)?;
    Ok(())
}

fn read_snapshot(path: &Path, id: &str) -> Result<Snapshot> {
    let mut input = BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("`{id}` is not a tgrep snapshot (or was written by another version)");
    }
    let created_at = read_u64(&mut input)?;
    let flags = read_u32(&mut input)?;
    let root = read_string(&mut input)?;
    let label = read_string(&mut input)?;
    let count = read_u64(&mut input)?;

    let mut files = BTreeMap::new();
    for _ in 0..count {
        let path = read_string(&mut input)?;
        let size = read_u64(&mut input)?;
        let mtime = read_u64(&mut input)?;
        let digest = match read_u8(&mut input)? {
            0 => None,
            _ => {
                let mut digest = [0; 16];
                input.read_exact(&mut digest)?;
                Some(digest)
            }
        };
        let hash = match read_u8(&mut input)? {
            0 => None,
            _ => {
                let mut hash = [0; 32];
                input.read_exact(&mut hash)?;
                Some(hash)
            }
        };
        files.insert(
            path,
            Entry {
                size,
                mtime,
                digest,
                hash,
            },
        );
    }
    Ok(Snapshot {
        id: id.to_string(),
        label,
        created_at,
        root,
        has_hashes: flags & FLAG_CONTENT_HASHES != 0,
        files,
    })
}

fn read_u8(input: &mut impl Read) -> Result<u8> {
    let mut buffer = [0; 1];
    input.read_exact(&mut buffer)?;
    Ok(buffer[0])
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut buffer = [0; 4];
    input.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes(buffer))
}

fn read_u64(input: &mut impl Read) -> Result<u64> {
    let mut buffer = [0; 8];
    input.read_exact(&mut buffer)?;
    Ok(u64::from_le_bytes(buffer))
}

fn read_string(input: &mut impl Read) -> Result<String> {
    let len = read_u32(input)? as usize;
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}

pub fn create(state: &McpState, args: &Value) -> Result<ToolOutput> {
    let with_hashes = args
        .get("hash")
        .and_then(Value::as_bool)
        .unwrap_or_default();
    let no_ignore = args
        .get("no_ignore")
        .and_then(Value::as_bool)
        .unwrap_or_default();
    let label = match args.get("label").and_then(Value::as_str) {
        Some(label) => sanitize_id(label)?,
        None => String::new(),
    };

    let started = std::time::Instant::now();
    let mut snapshot = observe(state, no_ignore, with_hashes)?;
    snapshot.label = label.clone();
    snapshot.id = if label.is_empty() {
        format!("snap-{}", snapshot.created_at)
    } else {
        label
    };

    let path = snapshot_path(state, &snapshot.id);
    if path.exists() && args.get("overwrite").and_then(Value::as_bool) != Some(true) {
        bail!(
            "snapshot `{}` already exists; pass overwrite=true to replace it",
            snapshot.id
        );
    }
    write_snapshot(&path, &snapshot)?;
    let elapsed = started.elapsed();

    let hashed_note = if with_hashes {
        "content hashes recorded: verification and rename detection are available"
    } else {
        "metadata only: pass hash=true to enable content verification and rename detection"
    };
    Ok(ToolOutput {
        text: format!(
            "Snapshot `{}` created: {} files in {:.1}s ({hashed_note}).\n",
            snapshot.id,
            snapshot.files.len(),
            elapsed.as_secs_f64()
        ),
        structured: json!({
            "snapshot": snapshot.describe(),
            "elapsed_ms": elapsed.as_millis() as u64,
            "note": hashed_note,
        }),
    })
}

pub fn list(state: &McpState, _args: &Value) -> Result<ToolOutput> {
    let dir = snapshots_dir(state);
    let mut snapshots = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(SNAPSHOT_EXT) {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string();
            match read_snapshot(&path, &id) {
                Ok(snapshot) => snapshots.push(snapshot.describe()),
                Err(error) => snapshots.push(json!({ "id": id, "error": error.to_string() })),
            }
        }
    }
    snapshots.sort_by_key(|snapshot| {
        std::cmp::Reverse(snapshot.get("created_at").and_then(Value::as_u64))
    });

    let text = if snapshots.is_empty() {
        "No snapshots. Create one with snapshot_create.\n".to_string()
    } else {
        snapshots
            .iter()
            .map(|snapshot| {
                format!(
                    "{}  {} files  {}  {}\n",
                    snapshot.get("id").and_then(Value::as_str).unwrap_or("?"),
                    snapshot.get("files").and_then(Value::as_u64).unwrap_or(0),
                    snapshot.get("age").and_then(Value::as_str).unwrap_or("?"),
                    if snapshot
                        .get("content_hashes")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        "hashed"
                    } else {
                        "metadata"
                    }
                )
            })
            .collect()
    };
    Ok(ToolOutput {
        text,
        structured: json!({ "snapshots": snapshots }),
    })
}

pub fn delete(state: &McpState, args: &Value) -> Result<ToolOutput> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
    let id = sanitize_id(id)?;
    let path = snapshot_path(state, &id);
    if !path.exists() {
        bail!("no snapshot `{id}`");
    }
    std::fs::remove_file(&path)?;
    Ok(ToolOutput {
        text: format!("Deleted snapshot `{id}`.\n"),
        structured: json!({ "deleted": id }),
    })
}

/// How a changed file was established to have changed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Evidence {
    Size,
    Digest,
    Mtime,
    Content,
}

impl Evidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Digest => "metadata-digest",
            Self::Mtime => "mtime",
            Self::Content => "content-hash",
        }
    }
}

struct Change {
    path: String,
    size: u64,
    previous_size: Option<u64>,
    evidence: Evidence,
}

impl Change {
    fn to_json(&self) -> Value {
        let mut value = json!({ "path": self.path, "size": self.size });
        let map = value.as_object_mut().expect("object literal");
        if let Some(previous) = self.previous_size {
            map.insert("previous_size".into(), previous.into());
            map.insert("evidence".into(), self.evidence.as_str().into());
        }
        value
    }
}

pub fn diff(state: &McpState, args: &Value) -> Result<ToolOutput> {
    let base_id = args
        .get("base")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`base` is required"))?;
    let base_id = sanitize_id(base_id)?;
    let base_path = snapshot_path(state, &base_id);
    if !base_path.exists() {
        bail!("no snapshot `{base_id}`; snapshot_list shows the available ones");
    }
    let base = read_snapshot(&base_path, &base_id)?;

    let verify = args
        .get("verify")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string();
    if !matches!(verify.as_str(), "none" | "suspect" | "all") {
        bail!("`verify` must be none, suspect or all");
    }
    let detect_renames = args
        .get("detect_renames")
        .and_then(Value::as_bool)
        .unwrap_or_default();
    let max_entries = args
        .get("max_entries")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_MAX_ENTRIES))
        .unwrap_or(DEFAULT_MAX_ENTRIES);
    let prefix = args
        .get("path")
        .and_then(Value::as_str)
        .map(|path| {
            let path = normalize(path.trim_matches('/'));
            if path.is_empty() {
                path
            } else {
                format!("{path}/")
            }
        })
        .filter(|prefix| !prefix.is_empty());

    let target_id = args.get("target").and_then(Value::as_str).unwrap_or("live");
    let started = std::time::Instant::now();
    // Only a live target can be hashed on demand; deleted files are gone.
    let target_is_live = target_id == "live";
    let target = if target_is_live {
        observe(state, false, verify == "all" && base.has_hashes)?
    } else {
        let id = sanitize_id(target_id)?;
        let path = snapshot_path(state, &id);
        if !path.exists() {
            bail!("no snapshot `{id}`");
        }
        read_snapshot(&path, &id)?
    };

    let keep = |path: &str| match &prefix {
        Some(prefix) => path.starts_with(prefix.as_str()),
        None => true,
    };

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for (path, entry) in target.files.iter().filter(|(path, _)| keep(path)) {
        match base.files.get(path) {
            None => added.push(Change {
                path: path.clone(),
                size: entry.size,
                previous_size: None,
                evidence: Evidence::Size,
            }),
            Some(previous) => {
                if let Some(evidence) = changed(previous, entry) {
                    modified.push(Change {
                        path: path.clone(),
                        size: entry.size,
                        previous_size: Some(previous.size),
                        evidence,
                    });
                }
            }
        }
    }
    for (path, entry) in base.files.iter().filter(|(path, _)| keep(path)) {
        if !target.files.contains_key(path) {
            deleted.push(Change {
                path: path.clone(),
                size: entry.size,
                previous_size: None,
                evidence: Evidence::Size,
            });
        }
    }

    let verification = verify_changes(
        state,
        &base,
        &target,
        &mut modified,
        &verify,
        target_is_live,
    )?;
    let renamed = if detect_renames {
        detect_renamed(state, &base, &mut added, &mut deleted, target_is_live)?
    } else {
        Vec::new()
    };

    let elapsed = started.elapsed();
    let truncated =
        added.len() > max_entries || modified.len() > max_entries || deleted.len() > max_entries;

    let mut structured = json!({
        "base": base.describe(),
        "target": if target_is_live {
            json!({ "id": "live", "files": target.files.len() })
        } else {
            target.describe()
        },
        "counts": {
            "added": added.len(),
            "modified": modified.len(),
            "deleted": deleted.len(),
            "renamed": renamed.len(),
        },
        "added": changes_json(&added, max_entries),
        "modified": changes_json(&modified, max_entries),
        "deleted": changes_json(&deleted, max_entries),
        "renamed": renamed,
        "truncated": truncated,
        "verification": verification,
        "elapsed_ms": elapsed.as_millis() as u64,
    });
    if truncated {
        let map = structured.as_object_mut().expect("object literal");
        map.insert(
            "by_directory".into(),
            by_directory(&added, &modified, &deleted),
        );
    }

    Ok(ToolOutput {
        text: render_diff(
            &base,
            target_id,
            &added,
            &modified,
            &deleted,
            &renamed,
            max_entries,
            truncated,
            &verification,
        ),
        structured,
    })
}

/// Whether two observations of the same path disagree, and on what grounds.
fn changed(previous: &Entry, current: &Entry) -> Option<Evidence> {
    if previous.size != current.size {
        return Some(Evidence::Size);
    }
    if let (Some(before), Some(after)) = (previous.hash, current.hash) {
        return (before != after).then_some(Evidence::Content);
    }
    if let (Some(before), Some(after)) = (previous.digest, current.digest) {
        return (before != after).then_some(Evidence::Digest);
    }
    (previous.mtime != current.mtime).then_some(Evidence::Mtime)
}

/// Re-check metadata-flagged changes against file content.
///
/// Only a base snapshot that recorded content hashes can support this: without
/// one there is nothing to compare today's bytes against, and saying so beats
/// reporting an unverified answer as verified.
fn verify_changes(
    state: &McpState,
    base: &Snapshot,
    target: &Snapshot,
    modified: &mut Vec<Change>,
    verify: &str,
    target_is_live: bool,
) -> Result<Value> {
    if verify == "none" {
        return Ok(json!({ "mode": "none", "available": false }));
    }
    if !base.has_hashes {
        return Ok(json!({
            "mode": verify,
            "available": false,
            "note": "base snapshot holds no content hashes; create it with hash=true to verify",
        }));
    }
    if !target_is_live && !target.has_hashes {
        return Ok(json!({
            "mode": verify,
            "available": false,
            "note": "target snapshot holds no content hashes",
        }));
    }

    let candidates: Vec<String> = if verify == "all" {
        modified.iter().map(|change| change.path.clone()).collect()
    } else {
        modified
            .iter()
            .filter(|change| change.evidence != Evidence::Size)
            .map(|change| change.path.clone())
            .collect()
    };
    let hashed = candidates.len();

    let current: BTreeMap<String, [u8; 32]> = candidates
        .into_par_iter()
        .filter_map(|path| {
            let hash = if target_is_live {
                hash_file(&state.root.join(&path)).ok()?
            } else {
                target.files.get(&path)?.hash?
            };
            Some((path, hash))
        })
        .collect();

    let before = modified.len();
    modified.retain(|change| {
        let (Some(expected), Some(actual)) = (
            base.files.get(&change.path).and_then(|entry| entry.hash),
            current.get(&change.path),
        ) else {
            return true;
        };
        expected != *actual
    });
    let cleared = before - modified.len();

    Ok(json!({
        "mode": verify,
        "available": true,
        "hashed": hashed,
        "cleared": cleared,
        "note": format!(
            "{cleared} of {hashed} metadata-flagged files had identical content and were dropped"
        ),
    }))
}

/// Pair deletions with additions that carry the same bytes.
fn detect_renamed(
    state: &McpState,
    base: &Snapshot,
    added: &mut Vec<Change>,
    deleted: &mut Vec<Change>,
    target_is_live: bool,
) -> Result<Vec<Value>> {
    if !base.has_hashes || !target_is_live || added.is_empty() || deleted.is_empty() {
        return Ok(Vec::new());
    }
    let added_hashes: BTreeMap<String, [u8; 32]> = added
        .par_iter()
        .filter_map(|change| {
            let hash = hash_file(&state.root.join(&change.path)).ok()?;
            Some((change.path.clone(), hash))
        })
        .collect();

    let mut by_hash: BTreeMap<[u8; 32], String> = BTreeMap::new();
    for change in deleted.iter() {
        if let Some(hash) = base.files.get(&change.path).and_then(|entry| entry.hash) {
            by_hash.insert(hash, change.path.clone());
        }
    }

    let mut renamed = Vec::new();
    let mut matched_sources = Vec::new();
    let mut matched_targets = Vec::new();
    for (path, hash) in &added_hashes {
        if let Some(source) = by_hash.remove(hash) {
            renamed.push(json!({ "from": source, "to": path }));
            matched_sources.push(source);
            matched_targets.push(path.clone());
        }
    }
    added.retain(|change| !matched_targets.contains(&change.path));
    deleted.retain(|change| !matched_sources.contains(&change.path));
    Ok(renamed)
}

fn changes_json(changes: &[Change], limit: usize) -> Value {
    json!(
        changes
            .iter()
            .take(limit)
            .map(Change::to_json)
            .collect::<Vec<_>>()
    )
}

/// Aggregate a large diff by directory, so a truncated list still says where
/// the change is concentrated.
fn by_directory(added: &[Change], modified: &[Change], deleted: &[Change]) -> Value {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for change in added.iter().chain(modified).chain(deleted) {
        let directory = match change.path.rsplit_once('/') {
            Some((parent, _)) => parent.to_string(),
            None => ".".to_string(),
        };
        *counts.entry(directory).or_default() += 1;
    }
    let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(TOP_DIRECTORIES);
    json!(
        ranked
            .into_iter()
            .map(|(path, changes)| json!({ "path": path, "changes": changes }))
            .collect::<Vec<_>>()
    )
}

#[allow(clippy::too_many_arguments)]
fn render_diff(
    base: &Snapshot,
    target_id: &str,
    added: &[Change],
    modified: &[Change],
    deleted: &[Change],
    renamed: &[Value],
    limit: usize,
    truncated: bool,
    verification: &Value,
) -> String {
    let mut out = format!("{} ({}) -> {}\n", base.id, age(base.created_at), target_id);
    let mut section = |marker: char, changes: &[Change]| {
        for change in changes.iter().take(limit) {
            out.push_str(&format!("{marker} {}\n", change.path));
        }
        if changes.len() > limit {
            out.push_str(&format!("{marker} … {} more\n", changes.len() - limit));
        }
    };
    section('+', added);
    section('~', modified);
    section('-', deleted);
    for rename in renamed {
        out.push_str(&format!(
            "> {} -> {}\n",
            rename.get("from").and_then(Value::as_str).unwrap_or("?"),
            rename.get("to").and_then(Value::as_str).unwrap_or("?"),
        ));
    }
    out.push_str(&format!(
        "\n{} added, {} modified, {} deleted",
        added.len(),
        modified.len(),
        deleted.len()
    ));
    if !renamed.is_empty() {
        out.push_str(&format!(", {} renamed", renamed.len()));
    }
    out.push_str(".\n");
    if truncated {
        out.push_str("Listing truncated; see by_directory for where changes cluster.\n");
    }
    if let Some(note) = verification.get("note").and_then(Value::as_str) {
        out.push_str(&format!("Verification: {note}\n"));
    }
    out
}
