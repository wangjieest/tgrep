# tgrep as an MCP server

`tgrep mcp` serves one repository to an AI agent over the
[Model Context Protocol](https://modelcontextprotocol.io), speaking JSON-RPC 2.0
on stdin/stdout. It exists for the case tgrep was built for: a repository large
enough that scanning it per query is not an option, queried repeatedly by an
agent whose context window is the scarce resource.

```bash
tgrep mcp /path/to/repo          # start an MCP server on stdio
```

The server starts a `tgrep serve` for the repository on a background thread
unless one is already running, so an agent gets indexed search without anyone
having to manage a daemon. That embedded server lives and dies with the MCP
process; a server you started yourself is adopted instead and left alone.

## Connecting a client

Claude Code:

```bash
claude mcp add tgrep -- tgrep mcp /path/to/repo
```

Any client that launches a stdio server:

```json
{
  "mcpServers": {
    "tgrep": {
      "command": "tgrep",
      "args": ["mcp", "/path/to/repo"]
    }
  }
}
```

Pass the repository path explicitly. A client launches the server with whatever
working directory it happens to have, so a bare `tgrep mcp` can end up serving
the wrong tree.

Protocol versions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05` are
accepted; an unrecognised one is answered with the newest, which is how a client
learns what to fall back to. Diagnostics go to stderr — stdout carries protocol
messages and nothing else.

### Options

| Flag | Effect |
|------|--------|
| `--no-auto-index` | Never start a server or build an index. Queries answer from an existing index, or by scanning. |
| `--exclude <DIR>` | Exclude a directory from the index the server builds (repeatable). |
| `--max-memory <MB>` | Memory budget for that build. Defaults to 50% of RAM. |
| `--max-cpu <PERCENT>` | Share of logical cores that build may use. Defaults to 50. |
| `--index-path <DIR>` | Read and write the index somewhere other than `./.tgrep`. |

`--no-require-git`, `--max-filesize` and `--no-ignore` are accepted too, and
apply to both the index this server builds and the queries against it, so the
two cannot disagree about which files exist.

## Tools

| Tool | Use it for |
|------|-----------|
| `search` | Regex or literal search; returns matching lines with paths, line numbers and optional context. |
| `count_matches` | Match counts per file, ranked. Sizes a query before you pay for its output. |
| `search_files` | Find files by path, glob, type or substring. |
| `index_status` | Whether queries are indexed or scanning, and how the server tracks changes. |
| `snapshot_create` | Record the tree's current state under a name. |
| `snapshot_diff` | What changed since a snapshot. |
| `snapshot_list` / `snapshot_delete` | Manage stored snapshots. |

### The intended order of work on a large repository

1. **`count_matches`** on the broad term. It reads the whole match set but
   returns only the distribution, so `handle` in a 500k-file monorepo costs a
   few dozen lines of output instead of tens of thousands.
2. **`search`** narrowed to what step 1 pointed at, via `path`, `glob` or
   `type`.
3. **`search_files`** when the question is about paths rather than content.

Prefer `literal: true` for symbols and for any string a human typed — it removes
a whole class of regex-escaping mistakes.

### Output budgets

Large result sets are the failure mode these tools are shaped around:

- `search` reports 60 matches by default (`max_results`, up to 1000) and at most
  10 matching lines per file (`max_per_file`). One generated file cannot crowd
  out the rest of the repository.
- A line longer than 512 bytes is clipped with a note saying how much was
  dropped, so a minified bundle costs one line, not a context window.
- A truncated answer still reports the **true** totals — `total_matches`,
  `files_with_matches` — plus `top_files`, the per-file breakdown of where the
  rest of the matches are. Truncation narrows the listing, never the count.

Every result carries both a human-readable `content[0].text` rendering and a
`structuredContent` object, so a client can use whichever it prefers.

### Knowing what answered

Every result embeds an `index` block:

```json
{ "state": "indexed", "server": "embedded" }
```

| `state` | Meaning |
|---------|---------|
| `indexed` | The trigram index answered. Fast. |
| `building` | The initial build is still running; this query scanned the tree. Correct, slower. |
| `scanning` | No usable index. Correct, slowest. |

`server` is `embedded` (this process started it), `external` (one was already
running) or `none`.

This is a report of what happened, not a freshness guarantee. The index tracks
the filesystem asynchronously, so a search issued moments after an edit can run
before the change has been indexed. That is the same caveat that applies to the
CLI, documented in [AGENTS.md](AGENTS.md#freshness).

### Opening a session on a repository that has never been indexed

Building the first index is fast; scanning the same tree without one is not.
Measured on a 371,547-file UE engine checkout (Windows, `--max-cpu 50`):

| | Time |
|---|---|
| Build the index (319,166 files admitted, 1.1M trigrams) | **97.6 s** |
| One whole-tree content search issued before it existed | **still running after 12 min**, when it was killed |

So a content query that arrives before the index does is not slightly slower —
it is a different order of magnitude, and it burns CPU for the entire time.
`search` and `count_matches` therefore **wait up to `wait_seconds` (3 by
default) and then refuse a whole-repository scan**, returning a message naming
the three ways forward instead of an answer that may never arrive:

```json
{
  "isError": true,
  "content": [{ "type": "text", "text": "tgrep: the index is not ready (building: initial index 41000/319166 files), and scanning the whole repository would read every file in it. Wait with index_status {\"wait_seconds\": 60}, narrow the query with `path`, or pass allow_scan=true to scan anyway." }]
}
```

The 3-second default means a small repository finishes indexing inside the
first call and nothing is ever refused. On a large one, open the session with:

```json
{ "name": "index_status", "arguments": { "wait_seconds": 120 } }
```

which returns as soon as the index is ready.

Three things are never refused, because none of them reads the whole corpus:

- a query with `path` set to a subdirectory — that is a scope you chose, and it
  costs what the subtree costs;
- `search_files`, which walks rather than reads;
- anything at all when no server is building an index (`--no-auto-index`, or no
  server running). Then scanning is the answer rather than a fallback, and
  refusing it would leave no way to search.

`allow_scan: true` overrides the refusal when you want the scan anyway.

## Snapshots and change diffs

`snapshot_create` walks the tree and records one observation per file — path,
size, whole-second mtime, and a 16-byte digest over every full-resolution field
the platform supplies (nanosecond modification time, birth time, and on unix
ctime, inode and device). No file content is read. `snapshot_diff` then compares
two observations and reports what was added, modified, deleted, and optionally
renamed.

```jsonc
// snapshot_create { "label": "before-refactor" }
// ... work happens ...
// snapshot_diff { "base": "before-refactor" }
{
  "counts": { "added": 1, "modified": 2, "deleted": 1, "renamed": 0 },
  "modified": [
    { "path": "src/lib.rs", "size": 2210, "previous_size": 2184, "evidence": "size" },
    { "path": "src/util.rs", "size": 990, "previous_size": 990, "evidence": "metadata-digest" }
  ]
}
```

`evidence` names what established the change: `size`, `metadata-digest`,
`mtime` (the fallback where digests are unavailable) or `content-hash`.

Snapshots live in `<index-dir>/snapshots/` and follow the same traversal rules as
indexing — `.gitignore` is respected, hidden files are included — but they are
*not* subject to the index's 64 MiB file-size cap, because a snapshot describes
the folder rather than the searchable corpus and metadata costs the same
whatever a file weighs.

### What metadata cannot see

A metadata-only snapshot answers "what changed" from timestamps, and timestamps
have a resolution. On Windows a file's write time advances with the system clock
tick, about 15.6 ms: **two writes of the same byte count within one tick are
indistinguishable from no write at all.** On unix the ctime comparison makes this
far less likely but a filesystem that reports coarse times has the same gap.

The diff also errs the other way, and more often: rewriting a file with
identical bytes, `p4 sync`, a branch switch that restores a file — all move
metadata without changing content, and are reported as modified.

Both directions are handled by recording content hashes:

```jsonc
// snapshot_create { "label": "base", "hash": true }   // reads every file once
// snapshot_diff   { "base": "base", "verify": "suspect" }
{
  "counts": { "modified": 1 },
  "verification": {
    "mode": "suspect", "available": true, "hashed": 4, "cleared": 3,
    "note": "3 of 4 metadata-flagged files had identical content and were dropped"
  }
}
```

| `verify` | Behaviour |
|----------|-----------|
| `none` (default) | Metadata only. No file is read. |
| `suspect` | Re-hash files flagged by metadata alone (not by a size change) and drop the ones whose content matches. Reports `hashed` and `cleared`. |
| `all` | Hash every file present in both observations. Reports `compared`, `cleared`, and `content_only` — files that changed leaving *no* metadata trace, which is the case only `all` can find. |

The two modes answer different questions, so they count different things.
`suspect` asks "were these flagged files really changed?" and can only ever
remove entries. `all` asks "what changed?" of every shared file, and can add
entries metadata never flagged:

```json
"verification": {
  "mode": "all", "available": true,
  "compared": 2, "cleared": 1, "content_only": 0,
  "note": "compared 2 files by content: 1 were flagged by metadata but identical, 0 changed with no metadata trace"
}
```

Verification requires a base snapshot created with `hash: true`; there is
nothing to compare today's bytes against otherwise. When it is unavailable the
result says so rather than reporting an unverified answer as verified:

```json
"verification": {
  "mode": "suspect",
  "available": false,
  "note": "base snapshot holds no content hashes; create it with hash=true to verify"
}
```

`detect_renames` pairs deletions with additions carrying identical content. It
has the same requirement, for the same reason: a deleted file cannot be hashed
now, so its hash has to come from the snapshot.

### Cost

A metadata snapshot is a metadata walk — the same one `tgrep serve` uses for
reconciliation: 2.2 s for 371,626 files, and 2.6 s to diff that against the live
tree. `hash: true` reads every file once, in parallel, and costs accordingly.
Diffs are pure comparison; `verify` adds reads proportional to how many files
were flagged, not to repository size.

A large diff truncates its path listings at `max_entries` (200 by default) and
adds `by_directory`, so a 5,000-file branch switch still tells you where the
change is concentrated.

## Measured on a large repository

A 371,547-file Unreal Engine checkout on Windows, index on a different drive
from the tree, `--max-cpu 50`, everything driven through the MCP tools:

| Call | Result size | Time |
|------|------------|-----:|
| First index build (319,166 files admitted, 1.11M trigrams) | 2.87 GiB index, 1.51 GiB peak | 97.6 s |
| Server start on the existing index | — | 2.8 s |
| `search` literal, rare symbol | 1,763 matches in 540 files | 263 ms |
| `search` literal, common symbol | 18,318 matches in 11,738 files | 799 ms |
| `search` regex `class \w+Component\b` | 4,649 matches in 3,081 files | 3.4 s |
| `search` literal + `type: ["cpp"]` | 92,661 matches in 27,045 files | 2.4 s |
| `count_matches` | 19,103 matches in 11,738 files | 1.2 s |
| `search_files` `type: ["cpp"]` | 184,790 files | 1.4 s |
| `index_status` | — | 3 ms |
| `snapshot_create`, metadata only | 371,626 files | 2.2 s |
| `snapshot_diff` against the live tree | no changes | 2.6 s |

Every `search` row returned 20 matches; the result sizes are what the repository
actually holds, reported alongside them. A pattern that reaches tens of
thousands of matches spends its time resolving candidate paths, not matching —
which is why `count_matches` and a narrowing `type`/`path` are worth a round
trip before a broad `search`.

That diff row is also a correctness check: two observations of an unchanged tree
report zero differences, so the metadata digest is stable rather than merely
sensitive.

The snapshot's 371,626 files against the index's 319,166 is the difference
between describing a folder and describing a corpus: 52,381 files whose
*content* turned out to be binary, which a metadata walk cannot detect because
it reads no content, plus the 79 files above the index's size cap. Files
rejected by extension are excluded from both.

## Boundaries

- **Read-only.** No tool writes to the repository. The only files created are
  the index and snapshots, both under the index directory.
- **Rooted.** Every path argument is canonicalised and required to resolve
  inside the served root; `..` and symlinks pointing out of the tree are
  refused. Snapshot labels are restricted to characters that cannot escape the
  snapshot directory.
- **One repository per server.** Run one per tree, as you would one
  `tgrep serve` per tree.
- **Tool errors reach the model.** A bad regex, a missing snapshot or an escaped
  path comes back as a tool result with `isError: true` and a message the model
  can act on, not as a transport error it never sees.

## Relationship to the CLI

The MCP tools drive the same entry points as the command line, so an MCP answer
and the equivalent `tgrep` invocation resolve identically — server, on-disk
index, or scan — and cannot drift apart. Anything the CLI documents about
freshness, ignore rules or index compatibility ([AGENTS.md](AGENTS.md),
[README.md](README.md)) applies here unchanged.

The tools deliberately expose a narrower surface than the CLI: the flags that
widen the corpus and force a full scan (`--no-ignore`, `-a`, `-E`, positive
globs beyond the index) are either absent or, in the case of globs, documented
as costing a scan. On a repository where scanning is affordable, use the CLI.
