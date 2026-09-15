//! End-to-end coverage for `tgrep mcp`: the stdio protocol itself, the search
//! tools, and snapshot diffing.
//!
//! Every session runs with `--no-auto-index`, so the tests exercise the
//! scanning path without waiting on a background index build.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tgrep"))
            .arg("mcp")
            .arg(root)
            .arg("--no-auto-index")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tgrep mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        session.notify("notifications/initialized");
        session
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "server closed stdout");
        serde_json::from_str(&line).expect("response is JSON")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let response = self.read();
        assert_eq!(response["id"], json!(id), "response id must match request");
        assert_eq!(response["jsonrpc"], "2.0");
        response
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }));
    }

    /// Call a tool and return its result, asserting it did not report an error.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = result["result"].clone();
        assert_eq!(
            result["isError"],
            json!(false),
            "{name} failed: {}",
            result["content"][0]["text"]
        );
        result
    }

    fn call_expecting_error(&mut self, name: &str, arguments: Value) -> String {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = result["result"].clone();
        assert_eq!(result["isError"], json!(true), "{name} unexpectedly passed");
        result["content"][0]["text"]
            .as_str()
            .expect("error text")
            .to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    needle();\n}\n").unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn needle() {}\npub fn other() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.md"), "needle appears here too\n").unwrap();
    dir
}

#[test]
fn handshake_reports_tools_and_protocol_version() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    let tools = session.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    for expected in [
        "search",
        "count_matches",
        "search_files",
        "index_status",
        "snapshot_create",
        "snapshot_list",
        "snapshot_diff",
        "snapshot_delete",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }

    // Every tool must carry a schema a client can validate arguments against.
    for tool in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
    }

    assert_eq!(session.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn unknown_protocol_version_is_answered_with_a_known_one() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    let response = session.request(
        "initialize",
        json!({ "protocolVersion": "1999-01-01", "capabilities": {} }),
    );
    assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(response["result"]["serverInfo"]["name"], "tgrep");
}

#[test]
fn unknown_method_is_a_protocol_error_but_an_unknown_tool_is_not() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    let response = session.request("resources/list", json!({}));
    assert_eq!(response["error"]["code"], json!(-32601));

    // A bad tool name is the model's mistake to recover from, so it comes back
    // as a result it can read rather than a transport error it cannot.
    let text = session.call_expecting_error("nope", json!({}));
    assert!(text.contains("unknown tool"), "{text}");
}

#[test]
fn notifications_get_no_response() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    // If the notification were answered, this ping would read that answer and
    // the id assertion inside `request` would fail.
    session.notify("notifications/cancelled");
    session.request("ping", json!({}));
}

#[test]
fn search_reports_matches_totals_and_index_state() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    let result = session.call("search", json!({ "pattern": "needle", "literal": true }));
    let structured = &result["structuredContent"];
    assert_eq!(structured["total_matches"], json!(3));
    assert_eq!(structured["files_with_matches"], json!(3));
    assert_eq!(structured["truncated"], json!(false));
    // Without an index or server the answer is still correct, and says so.
    assert_eq!(structured["index"]["state"], "scanning");

    let paths: Vec<&str> = structured["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"src/main.rs"), "{paths:?}");
    assert!(paths.contains(&"src/lib.rs"), "{paths:?}");
    assert!(paths.contains(&"notes.md"), "{paths:?}");

    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("src/main.rs"), "{text}");
    assert!(text.contains("3 matches in 3 files"), "{text}");
}

#[test]
fn search_truncates_but_still_counts_every_match() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    let result = session.call(
        "search",
        json!({ "pattern": "needle", "literal": true, "max_results": 1 }),
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["shown"], json!(1));
    assert_eq!(structured["total_matches"], json!(3));
    assert_eq!(structured["truncated"], json!(true));
    // A truncated answer has to say where the rest of the matches are.
    assert_eq!(structured["top_files"].as_array().unwrap().len(), 3);
    assert!(structured["next_step"].is_string());
}

#[test]
fn search_scopes_by_path_type_and_context() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    let scoped = session.call(
        "search",
        json!({ "pattern": "needle", "literal": true, "path": "src" }),
    );
    assert_eq!(scoped["structuredContent"]["total_matches"], json!(2));
    let paths: Vec<&str> = scoped["structuredContent"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    // Paths stay relative to the server root even when a subdirectory was
    // searched, so one tool's output can be fed to the next.
    assert!(
        paths.iter().all(|path| path.starts_with("src/")),
        "{paths:?}"
    );

    let typed = session.call(
        "search",
        json!({ "pattern": "needle", "literal": true, "type": ["md"] }),
    );
    assert_eq!(typed["structuredContent"]["total_matches"], json!(1));

    let context = session.call(
        "search",
        json!({ "pattern": "needle", "literal": true, "path": "src/main.rs", "context": 1 }),
    );
    let first = &context["structuredContent"]["matches"][0];
    assert_eq!(first["before"][0], "fn main() {");
    assert_eq!(first["after"][0], "}");
}

#[test]
fn count_matches_ranks_files_by_hit_count() {
    let dir = fixture();
    std::fs::write(
        dir.path().join("src/busy.rs"),
        "needle\nneedle\nneedle\nneedle\n",
    )
    .unwrap();
    let mut session = Session::start(dir.path());

    let result = session.call(
        "count_matches",
        json!({ "pattern": "needle", "literal": true }),
    );
    let files = result["structuredContent"]["files"].as_array().unwrap();
    assert_eq!(files[0]["path"], "src/busy.rs");
    assert_eq!(files[0]["matches"], json!(4));
    assert_eq!(result["structuredContent"]["total_matches"], json!(7));
}

#[test]
fn search_files_lists_and_filters_paths() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    let all = session.call("search_files", json!({}));
    assert_eq!(all["structuredContent"]["total"], json!(3));

    let rust = session.call("search_files", json!({ "type": ["rust"] }));
    assert_eq!(rust["structuredContent"]["total"], json!(2));

    let named = session.call("search_files", json!({ "contains": "MAIN" }));
    assert_eq!(named["structuredContent"]["files"], json!(["src/main.rs"]));
}

#[test]
fn paths_outside_the_root_are_refused() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    for escape in ["..", "../..", "src/../.."] {
        let text =
            session.call_expecting_error("search", json!({ "pattern": "needle", "path": escape }));
        assert!(text.contains("outside the server root"), "{escape}: {text}");
    }
}

#[test]
fn a_bad_pattern_is_reported_to_the_caller() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    let text = session.call_expecting_error("search", json!({ "pattern": "**[" }));
    assert!(text.contains("regex"), "{text}");
}

#[test]
fn snapshot_diff_reports_additions_changes_and_deletions() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    session.call("snapshot_create", json!({ "label": "base" }));

    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn needle() { todo!() }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/added.rs"), "fn added() {}\n").unwrap();
    std::fs::remove_file(dir.path().join("notes.md")).unwrap();

    let result = session.call("snapshot_diff", json!({ "base": "base" }));
    let structured = &result["structuredContent"];
    assert_eq!(structured["counts"]["added"], json!(1));
    assert_eq!(structured["counts"]["modified"], json!(1));
    assert_eq!(structured["counts"]["deleted"], json!(1));
    assert_eq!(structured["added"][0]["path"], "src/added.rs");
    assert_eq!(structured["modified"][0]["path"], "src/lib.rs");
    assert_eq!(structured["deleted"][0]["path"], "notes.md");

    let scoped = session.call("snapshot_diff", json!({ "base": "base", "path": "src" }));
    assert_eq!(scoped["structuredContent"]["counts"]["deleted"], json!(0));
}

#[test]
fn verification_needs_a_hashed_base_and_clears_untouched_files() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    session.call("snapshot_create", json!({ "label": "plain" }));
    session.call(
        "snapshot_create",
        json!({ "label": "hashed", "hash": true }),
    );

    // Rewrite identical bytes: metadata moves, content does not.
    let path = dir.path().join("src/lib.rs");
    let bytes = std::fs::read(&path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&path, &bytes).unwrap();

    let unverifiable = session.call(
        "snapshot_diff",
        json!({ "base": "plain", "verify": "suspect" }),
    );
    let verification = &unverifiable["structuredContent"]["verification"];
    assert_eq!(verification["available"], json!(false));
    assert!(
        verification["note"]
            .as_str()
            .unwrap()
            .contains("no content hashes"),
        "{verification}"
    );

    let unverified = session.call("snapshot_diff", json!({ "base": "hashed" }));
    assert_eq!(
        unverified["structuredContent"]["counts"]["modified"],
        json!(1)
    );

    let verified = session.call(
        "snapshot_diff",
        json!({ "base": "hashed", "verify": "suspect" }),
    );
    assert_eq!(
        verified["structuredContent"]["counts"]["modified"],
        json!(0)
    );
    assert_eq!(
        verified["structuredContent"]["verification"]["cleared"],
        json!(1)
    );
}

#[test]
fn renames_are_detected_from_a_hashed_snapshot() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    session.call("snapshot_create", json!({ "label": "base", "hash": true }));

    std::fs::rename(dir.path().join("notes.md"), dir.path().join("moved.md")).unwrap();

    let plain = session.call("snapshot_diff", json!({ "base": "base" }));
    assert_eq!(plain["structuredContent"]["counts"]["added"], json!(1));
    assert_eq!(plain["structuredContent"]["counts"]["deleted"], json!(1));

    let renamed = session.call(
        "snapshot_diff",
        json!({ "base": "base", "detect_renames": true }),
    );
    let structured = &renamed["structuredContent"];
    assert_eq!(structured["counts"]["renamed"], json!(1));
    assert_eq!(structured["counts"]["added"], json!(0));
    assert_eq!(structured["counts"]["deleted"], json!(0));
    assert_eq!(structured["renamed"][0]["from"], "notes.md");
    assert_eq!(structured["renamed"][0]["to"], "moved.md");
}

#[test]
fn snapshots_are_listed_deleted_and_name_checked() {
    let dir = fixture();
    let mut session = Session::start(dir.path());

    session.call("snapshot_create", json!({ "label": "one" }));
    let listed = session.call("snapshot_list", json!({}));
    assert_eq!(listed["structuredContent"]["snapshots"][0]["id"], "one");
    assert_eq!(
        listed["structuredContent"]["snapshots"][0]["files"],
        json!(3)
    );

    let clash = session.call_expecting_error("snapshot_create", json!({ "label": "one" }));
    assert!(clash.contains("already exists"), "{clash}");
    session.call(
        "snapshot_create",
        json!({ "label": "one", "overwrite": true }),
    );

    // A label becomes a file name, so anything that could escape the snapshot
    // directory is refused rather than sanitised into something else.
    for bad in ["../escape", "a/b", "with space", ""] {
        session.call_expecting_error("snapshot_create", json!({ "label": bad }));
    }

    session.call("snapshot_delete", json!({ "id": "one" }));
    let empty = session.call("snapshot_list", json!({}));
    assert_eq!(empty["structuredContent"]["snapshots"], json!([]));
    assert!(
        session
            .call_expecting_error("snapshot_diff", json!({ "base": "one" }))
            .contains("no snapshot")
    );
}

#[test]
fn index_status_names_the_root_and_the_index_it_would_read() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    let result = session.call("index_status", json!({}));
    let structured = &result["structuredContent"];
    assert_eq!(structured["state"], "scanning");
    assert_eq!(structured["server"], "none");
    assert!(
        structured["index_dir"]
            .as_str()
            .unwrap()
            .ends_with(".tgrep"),
        "{structured}"
    );
}

#[test]
fn verification_counts_what_it_actually_compared() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    session.call("snapshot_create", json!({ "label": "base", "hash": true }));

    // One real edit, and one rewrite of identical bytes that only moves
    // metadata. A verified diff must report exactly one change and say that it
    // cleared the other.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { needle(); todo!() }\n",
    )
    .unwrap();
    let untouched = dir.path().join("src/lib.rs");
    let bytes = std::fs::read(&untouched).unwrap();
    std::fs::write(&untouched, &bytes).unwrap();

    for mode in ["suspect", "all"] {
        let result = session.call("snapshot_diff", json!({ "base": "base", "verify": mode }));
        let structured = &result["structuredContent"];
        let verification = &structured["verification"];
        assert_eq!(
            structured["counts"]["modified"],
            json!(1),
            "{mode}: {structured}"
        );
        assert_eq!(structured["modified"][0]["path"], "src/main.rs", "{mode}");
        assert_eq!(verification["available"], json!(true), "{mode}");
        // The count has to describe the comparison that ran, not be re-derived
        // from whatever survived it.
        assert_eq!(verification["cleared"], json!(1), "{mode}: {verification}");
    }
}

#[test]
fn content_verification_finds_a_change_metadata_cannot_see() {
    let dir = fixture();
    let mut session = Session::start(dir.path());
    session.call("snapshot_create", json!({ "label": "base", "hash": true }));

    // Same byte count, and the modification time put back exactly as it was:
    // every field a metadata comparison has to work with is unchanged. This is
    // the sub-tick rewrite in slow motion.
    let path = dir.path().join("src/lib.rs");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] = b'P';
    std::fs::write(&path, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();

    let blind = session.call("snapshot_diff", json!({ "base": "base" }));
    assert_eq!(
        blind["structuredContent"]["counts"]["modified"],
        json!(0),
        "metadata should be unable to see this: {}",
        blind["structuredContent"]
    );

    let seen = session.call("snapshot_diff", json!({ "base": "base", "verify": "all" }));
    let structured = &seen["structuredContent"];
    assert_eq!(structured["counts"]["modified"], json!(1), "{structured}");
    assert_eq!(structured["modified"][0]["path"], "src/lib.rs");
    assert_eq!(structured["modified"][0]["evidence"], "content-hash");
    assert_eq!(structured["verification"]["content_only"], json!(1));
}
