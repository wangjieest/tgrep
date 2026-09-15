/// The search-shaped MCP tools: `search`, `search_files` and `count_matches`.
///
/// Each one drives the ordinary [`crate::search`] entry points through an
/// in-memory sink, so an MCP answer and the equivalent command line resolve the
/// same way — server, on-disk index, or scan — and cannot drift apart.
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::mcp::state::McpState;
use crate::output::{ColorMode, PathDisplay};
use crate::search::{self, SearchOptions};

/// Longest line body reported per match, in bytes. Long minified lines are the
/// single biggest way a search result can blow a context window.
const MAX_LINE_BYTES: usize = 512;
/// Matches reported when the caller names no limit.
const DEFAULT_MAX_RESULTS: usize = 60;
const MAX_MAX_RESULTS: usize = 1000;
/// Per-file cap applied when the caller names none, so one generated file
/// cannot crowd out every other hit.
const DEFAULT_MAX_PER_FILE: usize = 10;
/// Files listed in the `top_files` breakdown of a truncated result.
const TOP_FILES: usize = 8;

/// A `Write` sink whose bytes the caller can take back.
#[derive(Clone, Default)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Arguments shared by every search-shaped tool.
struct Scope {
    /// Absolute path the search runs over.
    path: std::path::PathBuf,
    /// Root-relative prefix, so reported paths are relative to the server root
    /// whatever subdirectory was searched.
    display: PathDisplay,
}

fn scope(state: &McpState, args: &Value) -> Result<Scope> {
    let requested = args.get("path").and_then(Value::as_str);
    let path = state.resolve_scope(requested)?;
    let display = match path.strip_prefix(&state.root) {
        Ok(rel) if rel.as_os_str().is_empty() => PathDisplay::Bare,
        Ok(rel) if path.is_file() => PathDisplay::Exact(slashed(&rel.to_string_lossy())),
        Ok(rel) => PathDisplay::Prefix(format!("{}/", slashed(&rel.to_string_lossy()))),
        Err(_) => PathDisplay::Bare,
    };
    Ok(Scope { path, display })
}

fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

fn base_options(state: &McpState, scope: &Scope, args: &Value) -> Result<SearchOptions> {
    let mut opts = SearchOptions {
        color: ColorMode::Never,
        // Root-relative forward slashes, so one tool's paths feed the next.
        path_separator: Some("/".to_string()),
        path_display: scope.display.clone(),
        field_match_separator: ":".to_string(),
        field_context_separator: "-".to_string(),
        max_filesize: state.opts.max_file_size,
        no_require_git: state.opts.no_require_git,
        hidden: flag(args, "hidden"),
        ..Default::default()
    };
    opts.glob = string_list(args, "glob")?;
    opts.types = string_list(args, "type")?;
    opts.types_not = string_list(args, "type_not")?;
    Ok(opts)
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn string_list(args: &Value, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(one)) => Ok(vec![one.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("`{key}` must contain strings"))
            })
            .collect(),
        Some(_) => bail!("`{key}` must be a string or an array of strings"),
    }
}

fn bounded(args: &Value, key: &str, default: usize, max: usize) -> Result<usize> {
    let Some(value) = args.get(key) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let value = value
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("`{key}` must be a non-negative integer"))?;
    Ok((value as usize).clamp(1, max))
}

fn apply_case(opts: &mut SearchOptions, args: &Value) -> Result<()> {
    match args.get("case").and_then(Value::as_str).unwrap_or("smart") {
        "smart" => opts.smart_case = true,
        "sensitive" => opts.case_sensitive = true,
        "insensitive" => opts.case_insensitive = true,
        other => bail!("`case` must be smart, sensitive or insensitive (got `{other}`)"),
    }
    Ok(())
}

/// Run one search and hand back everything the writer produced.
fn capture(
    state: &McpState,
    scope: &Scope,
    opts: &SearchOptions,
) -> Result<(String, std::time::Duration)> {
    let sink = SharedSink::default();
    let mut writer = search::new_writer_to(opts, Box::new(sink.clone()));
    let started = std::time::Instant::now();
    let result = search::run(&scope.path, state.index_path_arg(), opts, &mut writer);
    writer.finish()?;
    let elapsed = started.elapsed();
    result?;
    Ok((String::from_utf8_lossy(&sink.take()).into_owned(), elapsed))
}

pub fn search(state: &McpState, args: &Value) -> Result<ToolOutput> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`pattern` is required"))?;
    if pattern.is_empty() {
        bail!("`pattern` must not be empty");
    }
    let scope = scope(state, args)?;
    let max_results = bounded(args, "max_results", DEFAULT_MAX_RESULTS, MAX_MAX_RESULTS)?;
    let max_per_file = bounded(args, "max_per_file", DEFAULT_MAX_PER_FILE, MAX_MAX_RESULTS)?;
    let context = args
        .get("context")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(20) as usize;

    let mut opts = base_options(state, &scope, args)?;
    opts.pattern = pattern.to_string();
    opts.fixed_string = flag(args, "literal");
    opts.word_boundary = flag(args, "word");
    opts.multiline = flag(args, "multiline");
    opts.invert_match = flag(args, "invert");
    opts.json = true;
    opts.max_count = Some(max_per_file);
    opts.max_columns = Some(MAX_LINE_BYTES);
    opts.max_columns_preview = true;
    if context > 0 {
        opts.context = Some(context);
    }
    apply_case(&mut opts, args)?;

    let (raw, elapsed) = capture(state, &scope, &opts)?;
    let parsed = parse_events(&raw, max_results, context);

    let status = state.index_status();
    let truncated = parsed.total_matches > parsed.hits.len() as u64;
    let mut structured = json!({
        "matches": parsed.hits.iter().map(Hit::to_json).collect::<Vec<_>>(),
        "shown": parsed.hits.len(),
        "total_matches": parsed.total_matches,
        "files_with_matches": parsed.files_with_matches,
        "truncated": truncated,
        "elapsed_ms": elapsed.as_millis() as u64,
        "index": status.summary(),
    });
    let map = structured.as_object_mut().expect("object literal");
    if truncated {
        map.insert(
            "top_files".into(),
            json!(
                parsed
                    .top_files(TOP_FILES)
                    .into_iter()
                    .map(|(path, count)| json!({ "path": path, "matches": count }))
                    .collect::<Vec<_>>()
            ),
        );
        map.insert(
            "next_step".into(),
            json!(
                "Narrow with `path`, `glob` or `type`, or raise `max_results`. \
                 `count_matches` shows the full per-file distribution cheaply."
            ),
        );
    }

    Ok(ToolOutput {
        text: render_matches(&parsed, truncated, &status_line(&status)),
        structured,
    })
}

pub fn count_matches(state: &McpState, args: &Value) -> Result<ToolOutput> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`pattern` is required"))?;
    if pattern.is_empty() {
        bail!("`pattern` must not be empty");
    }
    let scope = scope(state, args)?;
    let max_files = bounded(args, "max_files", 40, 1000)?;

    let mut opts = base_options(state, &scope, args)?;
    opts.pattern = pattern.to_string();
    opts.fixed_string = flag(args, "literal");
    opts.word_boundary = flag(args, "word");
    opts.count = true;
    opts.count_matches = true;
    // A separator no path contains keeps parsing unambiguous.
    opts.field_match_separator = "\u{1}".to_string();
    apply_case(&mut opts, args)?;

    let (raw, elapsed) = capture(state, &scope, &opts)?;
    let mut counts: Vec<(String, u64)> = raw
        .lines()
        .filter_map(|line| line.rsplit_once('\u{1}'))
        .filter_map(|(path, count)| Some((path.to_string(), count.trim().parse().ok()?)))
        .collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let total: u64 = counts.iter().map(|(_, count)| count).sum();
    let files = counts.len();
    let shown: Vec<_> = counts.iter().take(max_files).collect();
    let status = state.index_status();

    let mut text = String::new();
    for (path, count) in &shown {
        text.push_str(&format!("{count:>6}  {path}\n"));
    }
    text.push_str(&format!(
        "\n{total} matches in {files} files{}. {}\n",
        if files > shown.len() {
            format!(" (showing top {})", shown.len())
        } else {
            String::new()
        },
        status_line(&status)
    ));

    Ok(ToolOutput {
        text,
        structured: json!({
            "files": shown
                .iter()
                .map(|(path, count)| json!({ "path": path, "matches": count }))
                .collect::<Vec<_>>(),
            "shown": shown.len(),
            "files_with_matches": files,
            "total_matches": total,
            "truncated": files > shown.len(),
            "elapsed_ms": elapsed.as_millis() as u64,
            "index": status.summary(),
        }),
    })
}

pub fn search_files(state: &McpState, args: &Value) -> Result<ToolOutput> {
    let scope = scope(state, args)?;
    let max_results = bounded(args, "max_results", 200, 5000)?;
    let contains = args
        .get("contains")
        .and_then(Value::as_str)
        .map(str::to_lowercase);

    let opts = base_options(state, &scope, args)?;
    let sink = SharedSink::default();
    let mut writer = search::new_writer_to(&opts, Box::new(sink.clone()));
    let started = std::time::Instant::now();
    search::list_files(&scope.path, state.index_path_arg(), &opts, &mut writer)?;
    writer.finish()?;
    let elapsed = started.elapsed();

    let raw = String::from_utf8_lossy(&sink.take()).into_owned();
    let all: Vec<&str> = raw
        .lines()
        .filter(|path| !path.is_empty())
        .filter(|path| match &contains {
            Some(needle) => path.to_lowercase().contains(needle),
            None => true,
        })
        .collect();
    let shown: Vec<&&str> = all.iter().take(max_results).collect();
    let status = state.index_status();

    let mut text: String = shown.iter().map(|path| format!("{path}\n")).collect();
    text.push_str(&format!(
        "\n{} files{}. {}\n",
        all.len(),
        if all.len() > shown.len() {
            format!(" (showing first {})", shown.len())
        } else {
            String::new()
        },
        status_line(&status)
    ));

    Ok(ToolOutput {
        text,
        structured: json!({
            "files": shown,
            "shown": shown.len(),
            "total": all.len(),
            "truncated": all.len() > shown.len(),
            "elapsed_ms": elapsed.as_millis() as u64,
            "index": status.summary(),
        }),
    })
}

fn status_line(status: &crate::mcp::state::IndexStatus) -> String {
    match &status.note {
        Some(note) => format!("[{}: {note}]", status.readiness.as_str()),
        None => format!("[{}]", status.readiness.as_str()),
    }
}

pub struct ToolOutput {
    pub text: String,
    pub structured: Value,
}

struct Hit {
    path: String,
    line: u64,
    column: Option<u64>,
    text: String,
    before: Vec<String>,
    after: Vec<String>,
}

impl Hit {
    fn to_json(&self) -> Value {
        let mut value = json!({
            "path": self.path,
            "line": self.line,
            "text": self.text,
        });
        let map = value.as_object_mut().expect("object literal");
        if let Some(column) = self.column {
            map.insert("column".into(), column.into());
        }
        if !self.before.is_empty() {
            map.insert("before".into(), json!(self.before));
        }
        if !self.after.is_empty() {
            map.insert("after".into(), json!(self.after));
        }
        value
    }
}

#[derive(Default)]
struct Parsed {
    hits: Vec<Hit>,
    total_matches: u64,
    files_with_matches: u64,
    per_file: Vec<(String, u64)>,
}

impl Parsed {
    fn top_files(&self, limit: usize) -> Vec<(String, u64)> {
        let mut files = self.per_file.clone();
        files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        files.truncate(limit);
        files
    }
}

/// Fold ripgrep's JSON event stream into the flat shape a model reads best.
///
/// Collection stops at `max_results`, but the per-file `end` events keep being
/// counted, so a truncated answer still reports the true total.
fn parse_events(raw: &str, max_results: usize, context: usize) -> Parsed {
    let mut parsed = Parsed::default();
    let mut pending_before: Vec<String> = Vec::new();
    let mut open_hit: Option<usize> = None;
    let mut last_line: u64 = 0;

    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let data = event.get("data").unwrap_or(&Value::Null);
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "begin" => {
                pending_before.clear();
                open_hit = None;
            }
            "match" => {
                let line_number = data
                    .get("line_number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                last_line = line_number;
                if parsed.hits.len() >= max_results {
                    open_hit = None;
                    pending_before.clear();
                    continue;
                }
                let text = clip(text_of(data));
                let column = data
                    .get("submatches")
                    .and_then(Value::as_array)
                    .and_then(|subs| subs.first())
                    .and_then(|sub| sub.get("start"))
                    .and_then(Value::as_u64)
                    .map(|start| start + 1);
                parsed.hits.push(Hit {
                    path: path_of(data),
                    line: line_number,
                    column,
                    text,
                    before: std::mem::take(&mut pending_before),
                    after: Vec::new(),
                });
                open_hit = Some(parsed.hits.len() - 1);
            }
            "context" => {
                let line_number = data
                    .get("line_number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let text = clip(text_of(data));
                // A context line continues the open hit only while adjacent.
                let continues = open_hit.is_some_and(|index| {
                    parsed.hits[index].after.len() < context && line_number == last_line + 1
                });
                if continues {
                    let index = open_hit.expect("checked above");
                    parsed.hits[index].after.push(text);
                    last_line = line_number;
                } else {
                    open_hit = None;
                    if context > 0 {
                        pending_before.push(text);
                        if pending_before.len() > context {
                            pending_before.remove(0);
                        }
                    }
                    last_line = line_number;
                }
            }
            "end" => {
                let matches = data
                    .get("stats")
                    .and_then(|stats| stats.get("matches"))
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                if matches > 0 {
                    parsed.files_with_matches += 1;
                    parsed.total_matches += matches;
                    parsed.per_file.push((path_of(data), matches));
                }
                open_hit = None;
                pending_before.clear();
            }
            _ => {}
        }
    }
    parsed
}

fn path_of(data: &Value) -> String {
    data.get("path")
        .and_then(|path| path.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn text_of(data: &Value) -> &str {
    data.get("lines")
        .and_then(|lines| lines.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Trim a line to a budget, on a char boundary, noting what was dropped.
fn clip(text: &str) -> String {
    let text = text.trim_end_matches(['\n', '\r']);
    if text.len() <= MAX_LINE_BYTES {
        return text.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [+{} bytes]", &text[..end], text.len() - end)
}

fn render_matches(parsed: &Parsed, truncated: bool, status: &str) -> String {
    if parsed.hits.is_empty() {
        return format!("No matches. {status}\n");
    }
    let mut out = String::new();
    let mut current = String::new();
    for hit in &parsed.hits {
        if hit.path != current {
            if !current.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{}\n", hit.path));
            current = hit.path.clone();
        }
        let first_before = hit.line.saturating_sub(hit.before.len() as u64);
        for (offset, line) in hit.before.iter().enumerate() {
            out.push_str(&format!("{:>6}- {line}\n", first_before + offset as u64));
        }
        out.push_str(&format!("{:>6}: {}\n", hit.line, hit.text));
        for (offset, line) in hit.after.iter().enumerate() {
            out.push_str(&format!("{:>6}- {line}\n", hit.line + 1 + offset as u64));
        }
    }
    out.push_str(&format!(
        "\n{} of {} matches in {} files{}. {status}\n",
        parsed.hits.len(),
        parsed.total_matches,
        parsed.files_with_matches,
        if truncated {
            " (truncated — narrow with path/glob/type, or raise max_results)"
        } else {
            ""
        }
    ));
    out
}
