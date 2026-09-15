/// `tgrep mcp` — a Model Context Protocol server over stdio.
///
/// Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout; every diagnostic goes
/// to stderr, because a stray byte on stdout corrupts the protocol stream.
mod query;
mod snapshot;
mod state;

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

pub use state::McpOptions;
use state::McpState;

/// Versions this server can speak, newest first. The newest is also what an
/// unrecognised request is answered with, which is how a client learns what to
/// downgrade to.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
tgrep serves one repository through a trigram index, so a regex search touches \
only the files that could match. It is built for repositories too large to scan.

Recommended order of work:
1. `count_matches` first on a broad symbol — it shows how many hits exist and \
which files hold them, for a fraction of the output of a full search.
2. `search` with `path`, `glob` or `type` narrowed to what step 1 pointed at. \
Use `literal: true` for symbols and strings; it avoids regex-escaping mistakes.
3. `search_files` to find files by path rather than content.

Every result carries an `index` block. `state: \"indexed\"` means the trigram \
index answered; `\"building\"` or `\"scanning\"` mean the tree is being read \
directly and the answer is correct but slow. Results reflect the index, which \
tracks the filesystem asynchronously — a search issued immediately after an \
edit may predate it.

The first index of a large repository takes a minute or two. Until it exists, \
a whole-repository content search reads every file and takes much longer than \
waiting, so `search` and `count_matches` refuse one and say so. Open such a \
session with `index_status {\"wait_seconds\": 120}`, or scope the query with \
`path`.

`snapshot_create` records the tree's metadata; `snapshot_diff` reports what has \
changed since. Pass `hash: true` when creating one to enable content \
verification and rename detection later.";

pub fn run(root: &Path, index_path: Option<&Path>, options: McpOptions) -> Result<()> {
    let mut state = McpState::new(root, index_path, options)?;
    eprintln!(
        "tgrep mcp: serving {}",
        crate::search::display_path(&state.root)
    );
    state.start_server();

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle_message(&line, &state) else {
            continue;
        };
        // One message per line: a client frames the stream by newline.
        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
}

/// Answer one incoming message, or `None` when it was a notification.
fn handle_message(line: &str, state: &McpState) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => return Some(error_response(Value::Null, -32700, &error.to_string())),
    };
    if !message.is_object() {
        // JSON-RPC batching was removed from MCP in 2025-06-18.
        return Some(error_response(
            Value::Null,
            -32600,
            "expected a single JSON-RPC object",
        ));
    }
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // A notification carries no id and must never be answered.
    let id = id?;

    let result: Result<Value> = match method {
        "initialize" => Ok(initialize(&params)),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => return Some(success(id, call_tool(&params, state))),
        "ping" => Ok(json!({})),
        other => {
            return Some(error_response(
                id,
                -32601,
                &format!("unknown method `{other}`"),
            ));
        }
    };
    match result {
        Ok(result) => Some(success(id, result)),
        Err(error) => Some(error_response(id, -32603, &format!("{error:#}"))),
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Echo a version we speak, else name ours and let the client decide.
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|known| **known == requested)
        .copied()
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "tgrep",
            "title": "tgrep — indexed code search",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Run a tool and shape its outcome as a `tools/call` result.
///
/// A failing tool is reported as a result with `isError`, not as a JSON-RPC
/// error: the model is the one that has to act on "bad regex" or "no such
/// snapshot", and a protocol error never reaches it.
fn call_tool(params: &Value, state: &McpState) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome = match name {
        "search" => query::search(state, &arguments),
        "search_files" => query::search_files(state, &arguments),
        "count_matches" => query::count_matches(state, &arguments),
        "index_status" => index_status(state, &arguments),
        "snapshot_create" => snapshot::create(state, &arguments),
        "snapshot_list" => snapshot::list(state, &arguments),
        "snapshot_diff" => snapshot::diff(state, &arguments),
        "snapshot_delete" => snapshot::delete(state, &arguments),
        other => Err(anyhow::anyhow!("unknown tool `{other}`")),
    };

    match outcome {
        Ok(output) => json!({
            "content": [{ "type": "text", "text": output.text }],
            "structuredContent": output.structured,
            "isError": false,
        }),
        Err(error) => json!({
            "content": [{ "type": "text", "text": format!("tgrep: {error:#}") }],
            "isError": true,
        }),
    }
}

fn index_status(state: &McpState, args: &Value) -> Result<query::ToolOutput> {
    let status = match args.get("wait_seconds") {
        Some(_) => state.await_ready(query::wait_seconds(args)),
        None => state.index_status(),
    };
    let root = crate::search::display_path(&state.root);
    let index_dir = crate::search::display_path(&state.index_dir);
    let mut structured = status.summary();
    let map = structured.as_object_mut().expect("object literal");
    map.insert("root".into(), root.as_str().into());
    map.insert("index_dir".into(), index_dir.as_str().into());
    if let Some(updated) = status.index_updated_at {
        map.insert("index_updated_at".into(), updated.into());
    }
    if let Some(server) = &status.server {
        map.insert("server_status".into(), server.clone());
    }

    let text = format!(
        "root: {root}\nindex: {index_dir}\nstate: {}{}\n",
        status.readiness.as_str(),
        match &status.note {
            Some(note) => format!("\nnote: {note}"),
            None => String::new(),
        }
    );
    Ok(query::ToolOutput { text, structured })
}

/// Shared argument fragments, so every tool describes scoping the same way.
fn scoping_properties() -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "path".into(),
        json!({
            "type": "string",
            "description": "Subdirectory or file under the server root to restrict the query to. Defaults to the whole root.",
        }),
    );
    properties.insert(
        "glob".into(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "Glob filters, e.g. [\"src/**/*.rs\"]. A leading '!' excludes. Positive globs can reinclude ignored files and then require a full scan.",
        }),
    );
    properties.insert(
        "type".into(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "File type filters, e.g. [\"rust\", \"py\"]. Cheaper than globs and always indexed.",
        }),
    );
    properties.insert(
        "type_not".into(),
        json!({ "type": "array", "items": { "type": "string" }, "description": "File types to exclude." }),
    );
    properties.insert(
        "hidden".into(),
        json!({ "type": "boolean", "default": false, "description": "Include hidden files and directories. Ignore rules still apply." }),
    );
    properties.insert(
        "wait_seconds".into(),
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": 600,
            "default": 3,
            "description": "How long to wait for a building index before answering. Raise it on a large repository whose first index is still being built.",
        }),
    );
    properties
}

fn matching_properties() -> serde_json::Map<String, Value> {
    let mut properties = scoping_properties();
    properties.insert(
        "pattern".into(),
        json!({ "type": "string", "description": "Regular expression, or exact text when literal is true." }),
    );
    properties.insert(
        "literal".into(),
        json!({ "type": "boolean", "default": false, "description": "Treat pattern as literal text. Prefer this for symbols and user-supplied strings." }),
    );
    properties.insert(
        "case".into(),
        json!({
            "type": "string",
            "enum": ["smart", "sensitive", "insensitive"],
            "default": "smart",
            "description": "smart: case-insensitive only when the pattern is all lowercase.",
        }),
    );
    properties.insert(
        "word".into(),
        json!({ "type": "boolean", "default": false, "description": "Match whole words only." }),
    );
    properties.insert(
        "allow_scan".into(),
        json!({
            "type": "boolean",
            "default": false,
            "description": "Run a whole-repository scan even when the index is not ready. Refused by default: on a large tree that reads every file and takes far longer than waiting for the index.",
        }),
    );
    properties
}

fn tool(name: &str, title: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
    })
}

fn object_schema(properties: serde_json::Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

fn tool_definitions() -> Vec<Value> {
    let mut search_properties = matching_properties();
    search_properties.insert(
        "context".into(),
        json!({ "type": "integer", "minimum": 0, "maximum": 20, "default": 0, "description": "Lines of context around each match." }),
    );
    search_properties.insert(
        "max_results".into(),
        json!({ "type": "integer", "minimum": 1, "maximum": 1000, "default": 60, "description": "Total matches to report. The true total is reported even when truncated." }),
    );
    search_properties.insert(
        "max_per_file".into(),
        json!({ "type": "integer", "minimum": 1, "default": 10, "description": "Matching lines to report per file, so one generated file cannot crowd out the rest." }),
    );
    search_properties.insert(
        "multiline".into(),
        json!({ "type": "boolean", "default": false, "description": "Let a match span line boundaries." }),
    );
    search_properties.insert(
        "invert".into(),
        json!({ "type": "boolean", "default": false, "description": "Report lines that do NOT match." }),
    );

    let mut count_properties = matching_properties();
    count_properties.insert(
        "max_files".into(),
        json!({ "type": "integer", "minimum": 1, "maximum": 1000, "default": 40, "description": "Files to list, ordered by match count." }),
    );

    let mut files_properties = scoping_properties();
    files_properties.insert(
        "contains".into(),
        json!({ "type": "string", "description": "Case-insensitive substring the path must contain." }),
    );
    files_properties.insert(
        "max_results".into(),
        json!({ "type": "integer", "minimum": 1, "maximum": 5000, "default": 200 }),
    );

    vec![
        tool(
            "search",
            "Search code",
            "Regex or literal search across the repository, answered from a trigram index. \
             Returns matching lines with paths, line numbers and optional context. Scope with \
             path/glob/type on a large repository; the reply reports the true match total even \
             when the listing is truncated.",
            object_schema(search_properties, &["pattern"]),
        ),
        tool(
            "count_matches",
            "Count matches per file",
            "Match counts per file, ordered by count — the cheap way to size a query and see \
             where hits cluster before running `search`. Reads the whole match set but returns \
             only the distribution.",
            object_schema(count_properties, &["pattern"]),
        ),
        tool(
            "search_files",
            "Find files by path",
            "List files that would be searched, filtered by glob, type or a path substring. \
             Answers from the index rather than walking the tree when it can.",
            object_schema(files_properties, &[]),
        ),
        tool(
            "index_status",
            "Index status",
            "Whether queries are answered from the index or by scanning, how far an initial \
             build has got, and how the server is watching for changes. Pass wait_seconds to \
             block until the index is ready — the right way to open a session on a large \
             repository that has not been indexed before.",
            object_schema(
                [(
                    "wait_seconds".to_string(),
                    json!({ "type": "integer", "minimum": 0, "maximum": 600, "description": "Wait up to this long for the index to become ready. Returns early once it is, or once nothing is building it." }),
                )]
                .into_iter()
                .collect(),
                &[],
            ),
        ),
        tool(
            "snapshot_create",
            "Snapshot the tree",
            "Record the current state of every file the walker admits (size, mtime and precise \
             change evidence) under a name, to diff against later. Metadata only by default; \
             hash=true also records content hashes, which is what makes verification and rename \
             detection possible — at the cost of reading every file once.",
            object_schema(
                [
                    ("label".to_string(), json!({ "type": "string", "description": "Name for the snapshot (letters, digits, '.', '_', '-'). Defaults to a timestamp." })),
                    ("hash".to_string(), json!({ "type": "boolean", "default": false, "description": "Also record a blake3 hash per file. Reads the whole tree." })),
                    ("no_ignore".to_string(), json!({ "type": "boolean", "default": false, "description": "Include files .gitignore would exclude." })),
                    ("overwrite".to_string(), json!({ "type": "boolean", "default": false, "description": "Replace an existing snapshot with the same label." })),
                ]
                .into_iter()
                .collect(),
                &[],
            ),
        ),
        tool(
            "snapshot_list",
            "List snapshots",
            "Existing snapshots for this repository, newest first, with their age, file count \
             and whether they carry content hashes.",
            object_schema(serde_json::Map::new(), &[]),
        ),
        tool(
            "snapshot_diff",
            "Diff against a snapshot",
            "What has been added, modified, deleted or renamed since a snapshot. Compares \
             against the tree as it is now by default, or against a second snapshot. \
             verify='suspect' re-reads metadata-flagged files and drops the ones whose content \
             is unchanged (needs a hashed base snapshot).",
            object_schema(
                [
                    ("base".to_string(), json!({ "type": "string", "description": "Snapshot id to compare from." })),
                    ("target".to_string(), json!({ "type": "string", "default": "live", "description": "'live' for the current tree, or another snapshot id." })),
                    ("verify".to_string(), json!({
                        "type": "string",
                        "enum": ["none", "suspect", "all"],
                        "default": "none",
                        "description": "Content verification. 'suspect' hashes files flagged by metadata alone; 'all' hashes every common file. Requires a base snapshot created with hash=true.",
                    })),
                    ("detect_renames".to_string(), json!({ "type": "boolean", "default": false, "description": "Pair deletions with additions carrying identical content. Requires a hashed base snapshot and a live target." })),
                    ("path".to_string(), json!({ "type": "string", "description": "Restrict the diff to this subdirectory." })),
                    ("max_entries".to_string(), json!({ "type": "integer", "minimum": 1, "maximum": 5000, "default": 200, "description": "Paths listed per category. Counts and a directory breakdown cover the rest." })),
                ]
                .into_iter()
                .collect(),
                &["base"],
            ),
        ),
        tool(
            "snapshot_delete",
            "Delete a snapshot",
            "Remove a stored snapshot.",
            object_schema(
                [(
                    "id".to_string(),
                    json!({ "type": "string", "description": "Snapshot id to delete." }),
                )]
                .into_iter()
                .collect(),
                &["id"],
            ),
        ),
    ]
}
