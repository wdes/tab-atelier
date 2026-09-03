# Session compaction (Claude Code transcripts)

Claude Code stores every session as an append-only JSONL transcript under
`~/.claude/projects/<slugified-cwd>/<session-uuid>.jsonl`. Each tab links to
one via its `agent_session_id` in `tabs.json`, and `claude --resume <uuid>`
replays the leaf→root path back to the API. These files **never shrink** —
Claude's own "compaction" summarises the *context window* (the tokens sent to
the API) but leaves the file on disk untouched and only ever growing.

This module is the complementary half: a **layered, clone-safe compactor** that
trims the on-disk JSONL while keeping `--resume` intact. It is a **disk +
faster-reload** optimisation, *not* a token/context reduction — see
[What it does and doesn't buy you](#what-it-does-and-doesnt-buy-you), which is
backed by measurements over real tabs.

- Engine: [`src/transcript_compact.rs`](../src/transcript_compact.rs)
- Dry-run runner: [`examples/transcript-compact.rs`](../examples/transcript-compact.rs)

> **Status: dry-run only.** The runner *measures* and *reports*; it never
> writes a transcript. Wiring the daemon `compact <tab>` op (atomic write +
> `.orig` backup + respawn) on top of the engine is deliberately not built yet.

## The transcript, briefly

A conversation is an append-only **tree**: every line carries a `uuid` and a
`parentUuid`, so branches (edits, retries) coexist and resume walks a single
leaf→root path. Two structures matter for trimming:

- A `compact_boundary` record (`type:"system"`, `subtype:"compact_boundary"`)
  marks where Claude summarised. Resume replays only from the **last** boundary
  forward — everything before it is scrollback the API never sees again. Its
  `compactMetadata` records `preTokens` / `postTokens` (the squash ratio).
- `toolUseResult` is Claude Code's own copy of a tool's output, sitting
  *alongside* the model-facing `tool_result` content block that `--resume`
  actually replays. It is metadata, not sent to the API.

## The trim layers

Each layer is independent ([`Config`]); a run activates any subset. Layers are
ordered lossless → aggressive.

| # | Layer | What it removes | Resume-safe because |
|---|-------|-----------------|---------------------|
| A | `dedup` | top-level `toolUseResult` when a `tool_result` block already carries the text | edits *inside* a record; uuid/parent untouched |
| B | `drop_file_history` | `file-history-snapshot` / `-delta` records (checkpoint machinery) | no `uuid`, never a `parentUuid` target → direct drop |
| C | `drop_attachments` | `attachment` records (context re-injected each turn) | spliced out — children re-parented to nearest surviving ancestor |
| D | `keep_thinking(K)` | `thinking` blocks on assistant turns older than the last **K** | historical thinking isn't replayed |
| E | `tool_cap(N)` | truncates `tool_result` content over **N** bytes to head+tail | in-record edit; a `…[trimmed N bytes]…` marker is left |
| F | `keep_images(K)` | blanks base64 image data on turns older than the last **K** | in-record edit |

Layers A/D/E/F edit content *inside* a record, so the message tree is
structurally identical. Layers B/C drop whole records; C re-parents any
surviving child onto the dropped record's nearest surviving ancestor.
[`validate`] then proves no `parentUuid` that resolved before the transform now
dangles.

### Presets

`presets()` bundles the layers into four named policies:

| preset | layers |
|--------|--------|
| `lossless` | A + B + C |
| `balanced` | + D `keep_thinking(6)` |
| `cap8k` | + E `tool_cap(8192)` |
| `aggressive` | D `keep_thinking(3)` + E `tool_cap(4096)` + F `keep_images(3)` |

## Clone-safety

[`apply`] takes **ownership** of the parsed records and mutates each
`serde_json::Value` in place — it `.take()`s the value out, edits it, and puts
it back. 100 MB+ files and multi-KB tool outputs are never deep-`.clone()`d.
Callers that try several configs re-`parse` from the retained raw text instead
of cloning `Value`s. [`measure`] / [`measure_batch`] are borrow-only:
`measure_batch` serialises each block **once** and scores every config from the
cached lengths, so scanning N configs costs one serialization per block, not N.

## Run the dry-run report

Build headless and run the example (release recommended for speed; debug works):

```sh
cargo run --release --no-default-features --features headless \
  --example transcript-compact -- <report|space|boundary|tab NAME|examples NAME>
```

| subcommand | what it shows |
|------------|---------------|
| `report` (default) | per-layer + preset reclaim across every live tab, with a resume-safety spot-check on the biggest tabs |
| `space` | where the bytes go: breakdown by record / content-block / attachment kind, plus the biggest single samples |
| `boundary` | post-compaction reality — pre- vs post-`compact_boundary` bytes, the squash ratio, and the post-boundary block breakdown |
| `tab NAME` | per-preset before/after for one tab |
| `examples NAME` | like `tab`, plus concrete examples of what each layer touches |

## What it does and doesn't buy you

Measured over 56 live tabs on this workstation (~712 MB of transcripts):

- **21 / 56 tabs have ever compacted.** The other 35 replay their whole file —
  but they're all small (largest ~9 MB) *precisely because* they haven't hit
  the ~1 M-token auto-compact threshold, so a full-replay tab is by definition
  ≤ the context window.
- **78 % of the bytes are pre-boundary scrollback (556 MB) the API never sees
  again.** Trimming there is pure disk/reload — zero effect on what Claude
  receives.
- **Compaction crushes the sent context to ~1 %.** Each boundary shows a
  ~1,000 K → ~12 K token squash (`compactMetadata`). A 104 MB tab that has
  compacted feeds only a few MB to the API.
- **Even the 22 % post-boundary region is mostly not sent.** Of that 156 MB,
  `toolUseResult` metadata (~37 MB) + `thinking`/signatures (~29 MB) ≈ 42 % is
  never replayed to the API.

So the honest framing: this compactor **reclaims disk and speeds reload**. It
reduces tokens/context sent to Claude only in the narrow case of a *no-boundary*
tab (layers E/F cap old tool outputs / images), which is bounded and rare — and
can occasionally rescue a session bumping the 32 MB request limit.

## See also

- [Agent resource probe](agent-probe.md) — per-tab CPU/RAM instrumentation.
- [Per-tab ssh-agent](ssh-agent.md) — the respawn model reused here for
  `net-off`/`net-on` (a future `compact <tab>` would follow the same pattern).
