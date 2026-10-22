# pdfterm

`pdfterm` is a low-latency PDF viewer for Kitty, Ghostty, and WezTerm. It renders where the command runs, compresses each page once, and sends the bitmap through the Kitty graphics protocol. Direct SSH sessions need no local rendering helper.

![pdfterm rendering an arXiv paper in dark mode inside Kitty](assets/pdfterm-dark-mode-arxiv.png)

The current page is kept when the file reloads. Omit the path, or press `f`, to open another PDF in a new tab.

## Requirements

- Rust 1.88 or newer to build
- Kitty 0.20+, Ghostty, or WezTerm with Kitty graphics support
- macOS arm64, Linux x86_64, or Linux aarch64

Run directly in the terminal; tmux graphics passthrough is not supported.

## Install

```console
cargo install --locked --git https://github.com/jrf/pdfterm.git
```

PDFium revision 7881 is checksummed and embedded. From a checkout, `cargo build --release`.

## Run

```console
pdfterm document.pdf
```

## Keys

Press `?` for the full list in the viewer.

| Key | Action |
| --- | --- |
| `j` `k` `↑` `↓` | Scroll vertically across pages |
| `h` `l` `←` `→` | Scroll horizontally, then change page |
| `Space` `PageDown` / `Backspace` `PageUp` | Page the viewport |
| `g` / `G` | First / last page |
| `:` | Go to a page |
| `/` | Search text, or filter the open list |
| `n` / `N` | Next / previous page with a match |
| `m` | Cycle fit: page, width, height |
| `+` / `-` | Zoom by 25% (up to 400%) |
| `0` | Reset zoom |
| `i` | Polaris-style dark mode |
| `p` | Render timings |
| `t` / `T` | Outline / theme for this session |
| `y` | Copy this page's text (OSC 52) |
| `Enter` / `L` | Link list / link list with highlights |
| `b` | Back from the last internal link |
| `f` | New tab |
| `Tab` / `Shift-Tab` | Next / previous tab |
| `Alt-1` … `Alt-9` | Jump to a tab |
| `x` / `Alt`/`Option` + click | PDF-to-source navigation |
| `q` | Leave link mode, or close the tab |
| `Esc` | Leave link mode, close a pane, clear a filter, or exit |

## Configuration

`$XDG_CONFIG_HOME/pdfterm/config.toml`, or `~/.config/pdfterm/config.toml`. First launch creates defaults; invalid configuration stops startup. See [config.default.toml](config.default.toml) for settings.

## Neovim

The bundled plugin works with [Lazy.nvim](https://github.com/folke/lazy.nvim); install the viewer separately:

```lua
{
  "jrf/pdfterm",
  main = "pdfterm",
  lazy = false,
  opts = { executable = "pdfterm", open_pdf = true, log_file = "~/.local/state/nvim/pdfterm.log" },
}
```

`:PdfTermOpen` opens a PDF. `:PdfTermForwardSplit` opens/navigates the viewer beside Neovim; `:PdfTermForward` navigates an existing viewer. `:PdfTermLog` opens the configured log file in a split. SyncTeX position checks, ambiguities, and navigation errors are recorded with timestamps and source/PDF coordinates.

Configured log-directory, open, write, and close failures report an explicit Neovim error without recursively logging it or aborting navigation.

Kitty splits require `allow_remote_control yes`; detached Neovim also needs a private `listen_on` socket. Ghostty control is macOS-only. WezTerm requires `wezterm` on `PATH`. For manual pairing, `:PdfTermViewerCommand` prints the Neovim-attached launch command and copies it to the clipboard.

For automatic local viewer splits over SSH, run `scripts/pdfterm-ssh HOST` from the client terminal. The remote needs Neovim, pdfterm, and its Neovim adapter. macOS remotes also need Python 3: their control connection uses a private, current-user-owned Unix socket relayed over SSH, without `netstat` or privileged port inspection. Linux remotes use a verified loopback reverse forward and require `ss`.

## SyncTeX (`synctex-parser`)

LaTeX navigation uses the built-in pure-Rust `synctex-parser` engine (no external `synctex` binary required on `PATH`) with a matching PDF/`.synctex(.gz)` build:

```console
latexmk -pdf -synctex=1 main.tex
```

Forward search uses the source cursor; `Alt`/`Option` + click or `x` navigates from PDF text back to its source. Neither direction changes terminal focus by default.

### Engine Specs
- **Companion discovery & caching**: Searches beside the PDF and in an optional `--build-directory` (`SYNCTEX_BUILD_DIRECTORY`), auto-detects gzip via `0x1f 0x8b` magic bytes regardless of extension, and caches parsed pages/structural maps per `DocumentRevision`.
- **Coordinates**: Emits top-down PDF bounding boxes (`SearchRect`) for forward jumps (with optional `--page-hint` tie-breaking) and returns 1-based `line`, 0-based UTF-8 `byte_column`, 1-based UTF-16 `column`, and 1-based Unicode scalar `column_char` for inverse hits.
- **Collected-body & macro heuristics**: Resolves deferred TeX anchors inside Beamer `frame` (including fragile `.vrb` remapping), `tabularx`, and multi-line `\caption{...}` blocks, plus preamble metadata (`\title`, `\author`, `\frametitle`, etc.), and refines inverse hits across prose (including TeX accents and OT1 spacing diacritics) and inline/display math.
- **Resource & safety bounds**: Opens companions and snapshots with nonblocking regular-file checks (rejecting FIFOs/symlinks), enforces a 128 MiB decoded stream cap and 512 box-depth limit, validates `i64` geometry bounds, and honors per-operation cancellation deadlines.

| Area | Baseline `synctex` (`libsynctex` CLI) | `synctex-parser` |
| --- | --- | --- |
| **Runtime & caching** | External C binary; re-reads and decompresses `.synctex.gz` on every call | Pure-Rust in-process library; `SyncTeXCache` reuses decompressed bytes and parsed page trees per `DocumentRevision` |
| **Granularity & units** | Line-level anchors (raw or missing column offsets) | Token- and math-refined positions with validated UTF-8 (`byte_column`), UTF-16 (`column`), and scalar (`column_char`) columns |
| **Collected bodies (`frame`, `tabularx`, `\caption`)** | Emits anchors only on the closing line (`\end{frame}`, `\end{tabularx}`, closing `}`) | Read-ahead resolves interior lines on forward search and remaps closing-line hits back to the interior source token on inverse search |
| **Beamer `.vrb` & metadata** | Inverse clicks open generated `.vrb` files or land on `\maketitle` / `\end{frame}` | Remaps `.vrb` hits back to the originating `.tex` frame and resolves `\title`, `\author`, `\frametitle`, etc. |
| **Source drift & ambiguity** | Silently picks the first candidate; assumes disk files match the PDF | Verifies revision-bound `SourceSnapshot` SHA-256 digests and distinguishes strict errors from warned `best_guess` results |
| **Malformed input & I/O safety** | Can block on FIFOs, ignore bad postambles, or overflow on extreme integers | `O_NONBLOCK` regular-file check, 128 MiB stream cap, 512 box-depth cap, `i64::MIN` rejection, and `Operation` deadlines |

The Neovim adapter tracks plugin builds with immutable source snapshots and native line-range anchors. Unchanged captured text follows insertions, deletions, undo, reload, and known buffer renames; changed or deleted targets use warned live boundaries. A successful plugin build captures saved inputs when compilation starts. Inverse clicks look up that build's matching PDF/SyncTeX revision before refinement, including after viewer reloads without a forward request. When no captured build exists, inverse navigation makes the usual saved-source word guess and explicitly warns that it assumes the source has not changed since compilation. It does not manufacture source history from current files. Build and forward commands use edited root directives without losing pre-save anchors. Restart Neovim and existing viewers after updating the plugin and executable together.

Automatic root discovery also uses the initiating buffer's complete live text, so adding or removing a standalone preamble selects the correct owner before saving. Explicit main selection and the first-20-line root-directive convention keep their precedence.

SyncTeX forward search uses warned compiler LINE geometry, not exact WORD ownership: saved-source hashes and matching text cannot prove which tokens arbitrary TeX hooks or macros printed. A captured build verifies source history, not TeX execution. The existing overlay/page selection is retained, but does not prove ownership of a particular printed word or table row. Without a usable line target, Neovim warns and leaves the current PDF position unchanged; it never substitutes the document beginning. Inverse search remains enabled; strict inverse search rejects competing targets and invalid source bounds. Invalid required source-cache data, missing PDFs, failed compilation, required-tool failures, and viewer/socket failures remain errors. A coarse forward warning does not enable best-guess inverse navigation in a standalone viewer.

A cursor inside a complete literal caption's long argument uses that argument's closing compiler line instead of its opening line. This avoids a preceding-graphic anchor when SyncTeX records the collected caption against its closing line. The same rule applies to readable captured-source anchors. Short caption options, comments, command definitions, verbatim text, and adjacent prose do not acquire caption scope. Beamer frame anchoring takes precedence. Navigation remains warned LINE geometry, not an exact word highlight.

Inverse queries preserve click-coordinate precision at source-region boundaries. Diagnostics distinguish missing mappings, ambiguous targets, and invalid readable source anchors; invalid anchors report the source path, requested line, and available line count.

Socket/command navigation leaves the clipboard untouched. `editor.transport = "none"` copies source locations via OSC 52; explicit `y` still copies page text.

## Stateful Neovim fuzzing

The edit campaign runs real headless Neovim, with a separate Python operation-history
oracle. It probes every UTF-8 byte position and line end after each edit, in both
directions. Histories include insert/replace/delete, duplicate text, line splits/joins,
undo/redo, save/unload/reload, and clean/dirty buffer renames. Checks cover occurrence
identity, precise coordinates versus required warnings, immutable snapshots, saved
bytes, unrelated dirty buffers, and actual inverse socket/cursor delivery.

```console
python3 scripts/nvim_source_fuzz.py --seed 20261011 --cases 1000000 --steps 32 --binary "$(command -v pdfterm)"
```

The default and maximum campaign size are 1,000,000 cases. Histories are generated
one at a time; use `--cases 16` for a short run. A full campaign starts Neovim for
each case, so plan for substantial runtime. Passing-case artifacts are discarded
by default; campaign totals and failure artifacts are retained. Use `--keep-passing`
only for small diagnostic campaigns that need every trace and observation.

Requires Python 3.9+, Neovim, and a built viewer. Each case has a process-group
deadline (`--timeout`, default 30 seconds). Results, failing seeds, edit traces, and
exact queries are retained under `target/nvim-fuzz-*`; semantic failures also produce
`repro.json` and a bounded, best-found `minimized.json` (`--minimize`, default 16
shrink attempts). Replay a trace with `--replay PATH`; `--output` must stay inside
this repository's `target/`.
Replay files are bounded to 8 MiB, captured source to 2 MiB, and worker results to
64 MiB. Invalid results fail explicitly with retained diagnostics. Output directories
containing previous case artifacts are rejected rather than reusing stale results.
Summary totals count observed queries and completed states/edits, not planned trace
coverage. Precision failures shrink only while preserving the violation class,
query, and original source owner.
The live `summary.json` is atomically updated after the first passing case and
every 100 cases, then finalized on completion or a retained failure.

The oracle never reads extmarks, `vim.diff`, or mapper results to establish expected
positions. The fixture is a valid metadata-only PDF, and listener setup uses an
explicit private plugin seam. This campaign checks source-history projection,
not rendered PDF/TeX ownership or public forward-command save/build behavior.
All byte positions are checked within generated states; arbitrary edit histories,
PDFs, compiler races, external reloads, and cold editor sessions are not exhaustively
covered. Warned coarse results must have valid bounds and identity; their exact
best-guess choice is not certified. Increasing cases does not prove arbitrary
macro-expanded TeX/math execution ownership in the PDF.
