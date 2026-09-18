# remarkable-agent-push

A security-audited, hardened build of `rr`, the CLI that lets an AI coding
agent push markdown to a reMarkable tablet as a native notebook.

## Provenance and credit

This repository is a derivative of
[`hiteshjoshi/remarkable_rust`](https://github.com/hiteshjoshi/remarkable_rust)
by Hitesh Joshi (MIT). The tool, its v6 notebook writer and its cloud sync
client are his work, and the full upstream commit history is preserved here
with original authorship. It is published as a standalone repository, not a
GitHub fork, so that the audit and hardening work is visible on its own.

What this repository adds is the layer around that tool: a source-level
security audit, a fix for a fail-open data-loss path, a rebuilt release
pipeline, and tighter guardrails for agent use.

| Change | Finding addressed | Author |
|---|---|---|
| Index parser fails closed; server-supplied index lines are re-emitted byte-for-byte; entry-count invariant before the root swap; previous root hash printed as a rollback handle; `rr push --dry-run` shows the exact root-index diff before anything is written | The parser silently skipped any index line it could not read, then rewrote the account-wide root index without it, which would drop that document from the library on the next push | Jeremiah Tims |
| Plaintext `tokens.json` dump removed and cleaned up on `auth` and `logout`; `config.toml` written owner-only (0700 directory, 0600 file) | Long-lived device token left in a world-readable debug file that survived logout | Matthew Miller ([upstream PR #5](https://github.com/hiteshjoshi/remarkable_rust/pull/5), merged here) |
| Markdown image embedding confined to the source directory, 10 MiB cap | Local file inclusion through the legacy upload path, reachable by injected markdown in an agent context | Matthew Miller (upstream PR #5) |
| Signed-upload redirects require HTTPS and an allowlisted storage host | Document bytes were PUT to any host named in a redirect | Matthew Miller (upstream PR #5) |
| Installer aborts without a verified SHA-256; no longer writes into agent configuration directories | Best-effort checksum; unrequested writes to `~/.claude`, `~/.opencode`, `~/.codex` | Matthew Miller (checksums, upstream PR #5); Jeremiah Tims (opt-in skills) |
| Table images positioned from sourced line heights | Tables overlapped the following heading on the device | Matthew Miller ([upstream PR #6](https://github.com/hiteshjoshi/remarkable_rust/pull/6), merged here) |
| GitHub Actions rebuilt: actions pinned to commit SHAs, read-only default token, tests gate the release build, native Intel macOS runner with a pinned deployment target | Upstream release workflow targeted a retired runner label and tag-pinned actions | Jeremiah Tims |
| `rr root-restore <hash>`: a plan-only rollback of the root pointer to an earlier root; `--yes` performs it under the same generation guard as a push | The previous-root hash a push printed could not be acted on without handling the bearer token by hand | Jeremiah Tims |
| `0.3.6-jt.4`: a document-API 401 is reported as a refusal that points to `rr status` first; `rr ls` reports entries it could not read and exits non-zero; `rr push` strips YAML frontmatter, strictly; `rr skills` refuses to overwrite a differing SKILL.md without `--force`; the page splitter ignores dash lines inside code fences; blank lines no longer inflate the table-position estimate | Every 401 was labeled "token expired"; a listing could silently omit entries; frontmatter landed as a stray first page; one command could replace a customized skill file; a fenced YAML example cut a page in two; uneven spacing around tables (fixed in part, the rest awaits on-device measurement) | Jeremiah Tims |

Finding codes: commit and tag messages cite the six `0.3.6-jt.4` fixes as
`F10` to `F15`, the numbers they carry in the source audit behind this
build. In the order of the last row above: `F10` the 401 message, `F11`
`rr ls`, `F13` frontmatter, `F14` `rr skills`, `F15` the page splitter,
`F12` table spacing (fixed in part).

Both upstream pull requests were unmerged upstream when they were merged
here (2026-09-17); each diff was read line by line first. Audit and patch
work was done with Claude Code as a pair programmer; commits carry a
`Co-Authored-By` trailer where that applies.

**Standing caveat.** `rr` speaks reMarkable's internal, undocumented sync
protocol. reMarkable can change it without notice. Keep your tablet synced
and treat any third-party cloud client, this one included, as something
that can break.

---

# rr: push anything to your reMarkable, from Claude / OpenCode / Codex

Tell your coding agent:

> *"Send this to my reMarkable."*

A few seconds later a **native reMarkable notebook** (handwriting-editable,
the yellow-icon kind, not a PDF) shows up on your tablet.

`rr` is a small Rust CLI that turns markdown into a native v6 reMarkable
notebook locally and uploads it via the device's own cloud sync API.
Upstream reports that it works on any reMarkable account, with or
without a Connect subscription; this build has only been tested on an
account with Connect active. Agents drive it through a SKILL file that `rr` installs for
Claude, OpenCode, and Codex.

---

## The agent flow (this is the main use case)

```bash
# 1. Install the binary (see Install below; checksum-verified)

# 2. Pair with reMarkable (one-time, browser-based)
rr auth

# 3. Opt in to the agent skill for the agent you use
rr skills --target claude --dry-run     # preview
rr skills --target claude               # install

# 4. Just ask. Restart the agent only if it does not see the new skill.
```

Claude Code watches its skills directory and normally picks a new skill
up inside the running session. If the agent does not see it (another
agent, or a skills directory that did not exist when the session
started), restart the agent process: close and reopen the CLI / app.

`rr skills` never replaces a SKILL.md that differs from the one it
ships: it refuses and names both files. Pass `--force` only if you mean
to overwrite your own edits.

In Claude / OpenCode / Codex, things like:

- *"Summarize this thread and push it to my remarkable."*
- *"Save these meeting notes for my tablet."*
- *"Send this research as something I can read on my reMarkable later."*
- *"Push the Q2 plan to my reMarkable in the background."*

The skill activates, the agent writes a clean markdown file, runs
`rr push`, and reports the document id. You pick up the tablet and the
document is already there, properly formatted, with real tables (more on
that below) and the title at the top.

### What the SKILL gets the agent to do

Tables render as real grids because `rr` rasterizes markdown tables to
PNG locally and embeds them as image blocks directly inside the v6
notebook page. Headings, paragraphs, and bullets ship as native typed
text. Split a document into pages by writing `---` between sections.

The SKILL also tells the agent to stay in Latin script (no fonts ship on
the device for CJK, Devanagari, Arabic, Cyrillic, so anything else
renders as tofu boxes), avoid emojis and ASCII art (the typed-text
engine drops or mangles them), and pick descriptive filenames with dates
so you can find docs on the tablet.

---

## Install

This repository publishes one pre-built binary: **Intel macOS
(`x86_64-apple-darwin`)**, built natively by GitHub Actions from a tree
whose tests passed on the same runner. Every other platform builds from
source. For the full upstream platform matrix, see the upstream project.

Preferred: download the archive and `SHA256SUMS` from the
[latest release](https://github.com/jtims/remarkable-agent-push/releases/latest),
verify, then place the binary yourself:

```bash
shasum -a 256 -c SHA256SUMS
tar xzf rr-x86_64-apple-darwin.tar.gz
install -m 0755 rr-x86_64-apple-darwin/rr ~/.local/bin/rr
```

Or read `install.sh` first and then run it. It refuses to install without
a matching SHA-256, drops the binary at `~/.local/bin/rr` (override with
`INSTALL_DIR=$HOME/bin`), and does **not** touch any agent's configuration
directory. Skills are opt-in through `rr skills`.

### From source (Rust installed)

```bash
git clone https://github.com/jtims/remarkable-agent-push.git
cd remarkable-agent-push
./install.sh --dev      # builds release and installs
```

### Platform support

The table and the Windows notes below are upstream's statements about the
upstream release binaries. This repository only builds and tests Linux
x86_64 and Intel macOS in CI, and only publishes the Intel macOS binary.
Upstream's own binaries do not contain the hardening described above.

| Platform | Arch | Status |
|----------|------|--------|
| macOS    | Apple Silicon (`aarch64-apple-darwin`) | working |
| macOS    | Intel (`x86_64-apple-darwin`) | working |
| Linux    | `x86_64-unknown-linux-gnu` | working |
| Linux    | `aarch64-unknown-linux-gnu` | working |
| Windows  | `x86_64-pc-windows-gnu` | binary builds + ships; `rr auth` flow tested via Git Bash / WSL, native PowerShell pairing is community-reported |

The release binary is statically linked from pure-Rust dependencies. No
cairo, no librsvg, no ImageMagick, no headless Chrome.

### Windows install

The bash installer above doesn't run on native Windows. Two options:

**Option A — manual download (PowerShell or File Explorer):**

1. Go to the upstream [latest release page](https://github.com/hiteshjoshi/remarkable_rust/releases/latest) (unhardened build; no Windows binary is published here).
2. Download `rr-x86_64-pc-windows-gnu.zip`.
3. Extract it. Inside is `rr.exe` and a `skills/` folder.
4. Move `rr.exe` somewhere on your `PATH` (e.g. `%USERPROFILE%\bin\` after
   adding that folder to PATH via *System Properties → Environment Variables*).
5. From a new PowerShell window:

   ```powershell
   rr auth
   rr skills --target all     # installs SKILL.md into ~/.claude, etc.
   ```

**Option B — WSL:** install the Linux binary inside WSL with the regular
`curl | bash` one-liner. Agents running in WSL pick up the skill the
same way.

Verify with `rr --version` once it's on your PATH.

---

## Tables that look like tables

The v6 typed-text engine on Paper Pro doesn't have a table primitive —
just paragraphs and bullets. So `rr` watches for markdown tables in the
source, renders each one to a PNG locally, and embeds the PNG as an
image block in the page right below the typed text. The agent just
writes ordinary markdown:

```markdown
| Item | Quantity |   Price |
|------|---------:|--------:|
| Pens |        3 |   $4.50 |
| Pads |        1 |     $12 |
| Tags |       24 |   $0.05 |
```

…and the device sees a sharp 1400-pixel grid with proper borders,
header weight, right-aligned numbers, and multi-line cell text. No
config, no flags.

The full pipeline, all in pure Rust:

```
markdown table  ─── pulldown-cmark events ───►  rows + alignments
                                                  │
                                                  │ build_table_svg
                                                  ▼
                                            SVG with borders, text,
                                            per-column widths, wrapping
                                                  │
                                                  │ usvg parses + lays out
                                                  │ text via system fonts
                                                  ▼
                                            usvg::Tree
                                                  │
                                                  │ resvg paints onto a
                                                  │ tiny-skia Pixmap
                                                  ▼
                                            1400×N PNG bytes
                                                  │
                                                  ▼
                          ImageRegistry + ImageItem block in the
                          page's v6 stream (sibling .png on disk)
```

Soft-wrapping happens before rasterization so long cells don't blow out
the grid. Column widths fit the longest cell, capped at 1400px to match
the reMarkable Paper Pro's reading area. Header rows get bold weight and
a thicker underline.

The same machinery is available for arbitrary diagrams: build the SVG
yourself, pipe it through `rr::raster::svg_to_png`, and `rr` will embed
the PNG as a page image.

---

## CLI reference

### One-time setup

```bash
rr auth                  # pair the machine with reMarkable cloud (browser-based)
rr status                # verify auth + cloud connectivity
rr logout                # forget credentials
```

### Pushing

```bash
rr push notes.md                         # markdown → native v6 notebook
rr push doc.md --title "Custom Title"    # override inferred title
rr push doc.md --device paper-pro-move   # also: paper-pro (default), rm2
rr push - --title "From stdin"           # read markdown from stdin
rr push doc.md --parent <FOLDER_UUID>    # land inside a folder (ids: rr ls --folders)
rr push doc.md --dry-run                 # show the root-index diff, upload nothing
```

`rr root-restore <hash>` rolls the root pointer back to an earlier root,
such as the `previous root` a push printed. It is plan-only by default:
it fetches the target index, refuses if that index does not parse, and
prints which lines would disappear and which would return. Add `--yes`
to perform it. The swap is guarded by the root generation, so a root that
moved in the meantime is refused, not overwritten.

`--dry-run` fetches the current root index, builds the one a real push
would upload (through the same code path), and prints the line-level
difference: how many existing lines are kept byte for byte, which line
would be added, and whether the order of existing lines is preserved. It
exits non-zero if any existing line would be dropped. Run it before the
first push to a library you care about.

On success `rr push` also prints `previous root` and `previous gen`: the
cloud root pointer as it stood before the push. Blobs are
content-addressed and never overwritten, so that hash identifies the
prior library state if a push ever needs to be undone.

Pushes land at the root of the device unless `--parent` is given, and
produce a native v6 notebook
the device renders directly — no cloud-side conversion step. Split the
markdown into multiple pages with `---` horizontal-rule lines.

### Library management

```bash
rr ls                    # list documents in the cloud
rr ls --folders          # only show folders
rr mkdir "Work/2026"     # create a folder   (see the note below)
rr rm <doc-uuid>         # delete by id      (see the note below)
```

`rr ls` reads the same sync v3 endpoints `push` uses. `rr mkdir` and
`rr rm` do not: they call the older document API (`/doc/v2/files`),
which answered HTTP 401 when this build was tested against a paired
Paper Pro account with Connect active (2026-09-17). Treat both as not
working. Create folders and delete documents on the tablet or in the
reMarkable app, and target an existing folder with `--parent`.
Rebuilding both commands on sync v3 is planned.

### Legacy: EPUB → cloud convert

There's also a hidden `rr connect-push` command that builds an EPUB
locally and posts it to the reMarkable cloud's EPUB → notebook converter
(the original v0.2 pipeline). It's kept only as a fallback; `rr push`
produces the same native notebook with no cloud-side conversion.
`connect-push` is also the one with `--background` plus `rr jobs`,
`rr logs`, `rr cancel` for detached uploads.

### Skill management

```bash
rr skills --target all          # install SKILL.md into claude/opencode/codex
rr skills --target claude       # one agent
rr skills --dry-run --target all
rr skills --target claude --force   # replace a SKILL.md that differs (refused otherwise)
```

The SKILL files document the upload pipeline plus what renders well on
the device and what doesn't, so agents make the right choices when
generating content.

---

## What `rr push` actually does

```
your.md
       │
       │  pulldown-cmark events → typed text + tables
       ▼
  RootText block (paragraphs / bullets / headings)
  + per-table PNG → ImageRegistry + ImageItem blocks
       │
       │  rr writes binary v6 streams
       ▼
  Per-page .rm v6 files + .metadata, .content, .pagedata
       │
       │  SHA-256 content-address every blob
       │  PUT /sync/v3/files/<hash>      (per blob)
       │  PUT /sync/v3/files/<doc-index>
       │  PUT /sync/v3/root              (412-retry on race)
       ▼
  Notebook appears on every paired device on next sync
```

The binary v6 format is the same one the device writes to its own
filesystem, so the cloud has nothing to convert — it just stores and
hands the blobs back to the tablet. That is why, according to upstream, this
path works without a Connect subscription (not tested here): it's the
same sync protocol every reMarkable device speaks to
`internal.cloud.remarkable.com`.

Some details:

- Markdown tables are rasterized to PNG locally and embedded as v6 image
  blocks. Pipeline: SVG → `usvg` → `resvg` → `tiny-skia` → PNG.
- `--device {paper-pro|paper-pro-move|rm2}` picks the page dimensions
  and text-frame geometry. Default is Paper Pro.
- Splitting on `---` produces a multi-page notebook with one chunk per
  page.

See [`skills/claude/SKILL.md`](skills/claude/SKILL.md) for the record of
what renders well on the device.

---

## Limitations

- One-way. Local → cloud. No download path.
- No update-in-place. Every push creates a new document; re-pushing the
  same source makes a duplicate. Delete the old one first, on the tablet
  or in the reMarkable app.
- Folder targeting is by id, not by name: `rr push --parent <FOLDER_UUID>`
  with ids from `rr ls --folders`.
- `rr mkdir` and `rr rm` do not work in practice. They use the document
  API, which upstream source comments describe as requiring a Connect
  subscription and which returned HTTP 401 in testing even with one. From
  `0.3.6-jt.4` that 401 is reported as a refusal that points to
  `rr status`, not as an expired token.
- No inline emphasis. The v6 typed-text engine on Paper Pro doesn't have
  inline bold/italic/code styling; the text arrives, just without the
  styling. Code blocks, images embedded in markdown, and footnotes are
  silently skipped today.
- No native fonts for non-Latin scripts on the device. Anything outside
  Latin renders as tofu.

---

## Privacy and data flow

Your reMarkable device and user tokens are stored locally at
`~/Library/Application Support/rr/config.toml` (macOS) or
`~/.config/rr/config.toml` (Linux), written owner-only (directory 0700,
file 0600). The user token is also mirrored to the OS keychain on macOS
and Windows. Nothing else holds credentials: the legacy `tokens.json`
debug file is no longer written and is deleted on `rr auth` and
`rr logout` if an older build left one behind.

The binary contacts reMarkable hosts only:

- `webapp-prod.cloud.remarkable.engineering`: pairing and token refresh.
- `internal.cloud.remarkable.com`: sync v3, the endpoint every reMarkable
  device talks to. Pushes, listings and the root pointer go here.
- `web.<region>.tectonic.remarkable.com`: the regional document API used
  by `mkdir`, `rm` and the legacy pipeline.
- The storage host named in a signed-upload redirect, which must be HTTPS
  and on an allowlist.

No analytics, no telemetry, no third-party services. `install.sh` also
contacts GitHub to download the release.

---

## License

MIT.

---

## Acknowledgements

- The reMarkable team for shipping a great device and a usable cloud
  API.
- The [`rmscene`](https://github.com/ricklupton/rmscene) project, whose
  reverse-engineering of the v6 binary format made the native pipeline
  possible.
- The "Read on reMarkable" Chrome extension for shipping source maps,
  which sped up the original EPUB-pipeline reverse-engineering (now the
  hidden `connect-push` fallback).
- The `usvg` / `resvg` / `tiny-skia` crates — without them "tables to
  PNG" wouldn't be a 50-line module.
