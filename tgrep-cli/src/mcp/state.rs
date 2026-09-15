/// Shared state for one `tgrep mcp` process: the served root, the index it
/// reads, and the server it talks to (possibly one it started itself).
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tgrep_core::builder;
use tgrep_core::meta::IndexMeta;

use crate::serve::{self, ServerInfo};

/// How this process obtained the server its queries reach, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServerOrigin {
    /// A server was already running for this index directory.
    External,
    /// This process started one on a background thread.
    Embedded,
    /// None was running and none was started.
    None,
}

pub struct McpOptions {
    /// Start a server for the root when none is running.
    pub auto_index: bool,
    pub exclude_dirs: Vec<String>,
    pub no_ignore: bool,
    pub no_require_git: bool,
    pub max_file_size: Option<u64>,
    pub memory_cap_bytes: u64,
    pub index_threads: usize,
}

pub struct McpState {
    pub root: PathBuf,
    pub index_path: Option<PathBuf>,
    pub index_dir: PathBuf,
    pub opts: McpOptions,
    origin: ServerOrigin,
    /// Why the embedded server stopped, once it has.
    server_error: Arc<Mutex<Option<String>>>,
}

impl McpState {
    pub fn new(root: &Path, index_path: Option<&Path>, opts: McpOptions) -> Result<Self> {
        let root = std::fs::canonicalize(root)
            .map_err(|e| anyhow::anyhow!("cannot open root {}: {e}", root.display()))?;
        if !root.is_dir() {
            bail!("root {} is not a directory", root.display());
        }
        let index_dir = index_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| builder::default_index_dir(&root));
        Ok(Self {
            root,
            index_path: index_path.map(Path::to_path_buf),
            index_dir,
            opts,
            origin: ServerOrigin::None,
            server_error: Arc::new(Mutex::new(None)),
        })
    }

    pub fn index_path_arg(&self) -> Option<&Path> {
        self.index_path.as_deref()
    }

    /// Adopt a running server, or start one on a background thread.
    ///
    /// An embedded server lives and dies with this process, so a client that
    /// launched `tgrep mcp` leaves nothing behind. Startup never fails the
    /// session: without a server, queries still answer from the on-disk index
    /// or by scanning, which [`Self::index_status`] reports as such.
    pub fn start_server(&mut self) {
        if server_reachable(&self.index_dir) {
            self.origin = ServerOrigin::External;
            return;
        }
        if !self.opts.auto_index {
            return;
        }

        let root = self.root.clone();
        let index_path = self.index_path.clone();
        let exclude = self.opts.exclude_dirs.clone();
        let no_ignore = self.opts.no_ignore;
        let no_require_git = self.opts.no_require_git;
        let max_file_size = self.opts.max_file_size;
        let memory_cap_bytes = self.opts.memory_cap_bytes;
        let index_threads = self.opts.index_threads;
        let failure = Arc::clone(&self.server_error);

        let spawned = std::thread::Builder::new()
            .name("tgrep-mcp-serve".into())
            .spawn(move || {
                let result = serve::run(
                    &root,
                    index_path.as_deref(),
                    serve::ServeOptions {
                        no_watch: false,
                        watch_mode: serve::WatchMode::Auto,
                        poll_interval: Duration::from_secs(120),
                        watch_budget: 8192,
                        exclude_dirs: &exclude,
                        memory_cap_bytes,
                        index_threads,
                        no_ignore,
                        no_require_git,
                        max_file_size,
                        auto_save_mutations: None,
                        watcher_queue_cap: None,
                    },
                );
                if let Err(error) = result {
                    eprintln!("tgrep mcp: embedded server stopped: {error}");
                    *failure.lock().unwrap() = Some(error.to_string());
                }
            });

        match spawned {
            Ok(_) => self.origin = ServerOrigin::Embedded,
            Err(error) => {
                eprintln!("tgrep mcp: could not start embedded server: {error}");
                *self.server_error.lock().unwrap() = Some(error.to_string());
            }
        }
    }

    /// Resolve a caller-supplied path against the root, rejecting anything that
    /// resolves outside it.
    ///
    /// Canonicalising before the containment check is what makes it hold: a
    /// prefix test on the literal argument passes for `..` and for a symlink
    /// pointing out of the tree.
    pub fn resolve_scope(&self, requested: Option<&str>) -> Result<PathBuf> {
        let Some(requested) = requested.map(str::trim).filter(|p| !p.is_empty()) else {
            return Ok(self.root.clone());
        };
        let joined = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.root.join(requested)
        };
        let resolved = std::fs::canonicalize(&joined)
            .map_err(|e| anyhow::anyhow!("cannot open path `{requested}`: {e}"))?;
        if !resolved.starts_with(&self.root) {
            bail!("path `{requested}` resolves outside the server root");
        }
        Ok(resolved)
    }

    /// What a query issued right now would actually read, and how fresh it is.
    pub fn index_status(&self) -> IndexStatus {
        let server = ServerInfo::load(&self.index_dir)
            .ok()
            .and_then(|info| crate::status::query_server_status_value(&info).ok());

        if let Some(server) = server {
            let indexing = server
                .get("indexing")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let hidden_complete = server
                .get("hidden_complete")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let progress = server
                .get("index_progress")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let total = server
                .get("index_total")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let state = if indexing {
                IndexReadiness::Building
            } else if hidden_complete {
                IndexReadiness::Indexed
            } else {
                IndexReadiness::Scanning
            };
            let note = match state {
                // A build with no published file count reports 0/0, reading as idle.
                IndexReadiness::Building if total == 0 => Some(
                    "initial index build starting; queries scan the tree until it completes"
                        .to_string(),
                ),
                IndexReadiness::Building => Some(format!(
                    "initial index {progress}/{total} files; queries scan the tree until it completes"
                )),
                IndexReadiness::Scanning => Some(
                    "server is up but has not published complete file coverage yet; queries scan"
                        .to_string(),
                ),
                IndexReadiness::Indexed => None,
            };
            return IndexStatus {
                readiness: state,
                origin: self.origin,
                note,
                server: Some(server),
                index_updated_at: None,
                server_error: self.server_error.lock().unwrap().clone(),
            };
        }

        let (readiness, note, updated_at) = match IndexMeta::load(&self.index_dir) {
            Ok(meta) if meta.complete && meta.hidden_complete => (
                IndexReadiness::Indexed,
                Some(
                    "no server running; answering from the on-disk index, which is a snapshot \
                     of its last build"
                        .to_string(),
                ),
                Some(meta.updated_at),
            ),
            Ok(meta) => (
                IndexReadiness::Scanning,
                Some(
                    "on-disk index is incomplete or predates hidden-file coverage; queries scan"
                        .to_string(),
                ),
                Some(meta.updated_at),
            ),
            Err(_) => (
                IndexReadiness::Scanning,
                Some(format!(
                    "no index at {}; queries scan every file",
                    crate::search::display_path(&self.index_dir)
                )),
                None,
            ),
        };
        IndexStatus {
            readiness,
            origin: self.origin,
            note,
            server: None,
            index_updated_at: updated_at,
            server_error: self.server_error.lock().unwrap().clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IndexReadiness {
    /// Queries use the trigram index.
    Indexed,
    /// The initial build is still running; queries scan.
    Building,
    /// No usable index; queries scan every file.
    Scanning,
}

impl IndexReadiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::Building => "building",
            Self::Scanning => "scanning",
        }
    }
}

pub struct IndexStatus {
    pub readiness: IndexReadiness,
    pub origin: ServerOrigin,
    pub note: Option<String>,
    pub server: Option<serde_json::Value>,
    pub index_updated_at: Option<u64>,
    pub server_error: Option<String>,
}

impl IndexStatus {
    /// The per-response summary every tool result carries, so a caller always
    /// knows whether it read an index or a filesystem scan.
    pub fn summary(&self) -> serde_json::Value {
        let mut summary = serde_json::json!({
            "state": self.readiness.as_str(),
            "server": match self.origin {
                ServerOrigin::External => "external",
                ServerOrigin::Embedded => "embedded",
                ServerOrigin::None => "none",
            },
        });
        let map = summary.as_object_mut().expect("object literal");
        if let Some(note) = &self.note {
            map.insert("note".into(), note.as_str().into());
        }
        if let Some(error) = &self.server_error {
            map.insert("server_error".into(), error.as_str().into());
        }
        summary
    }
}

fn server_reachable(index_dir: &Path) -> bool {
    let Ok(info) = ServerInfo::load(index_dir) else {
        return false;
    };
    crate::status::query_server_status_value(&info).is_ok()
}
