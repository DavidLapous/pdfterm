# pdfterm

`pdfterm` is a low-latency PDF viewer for Kitty terminals. It renders on the machine where the command runs, compresses each page once, and sends the bitmap through Kitty's graphics protocol. Direct SSH sessions need no local helper.

This is an experimental fork of [jrf/pdfterm](https://github.com/jrf/pdfterm),
adding continuous animated scrolling and editor-neutral SyncTeX integration.
It is not a sandboxed viewer for hostile PDFs.

![pdfterm rendering an arXiv paper in dark mode inside Kitty](assets/pdfterm-dark-mode-arxiv.png)

The current viewer fits one page to the terminal, keeps the current and adjacent pages in memory, and gives foreground renders priority over prefetch work. It reloads each open document automatically when the PDF changes while preserving that tab's current page. Run it without a path or press `f` to open a fuzzy PDF picker in a new tab; recently opened documents appear at the top and remain searchable alongside recursively discovered PDFs. Picker searches match filenames and parent directories, with filename matches ranked first. Press `/` to filter a picker; `Esc` clears an active query before closing it.

You can fit pages to the terminal width or height and scroll through the overflow, zoom in and out beyond the fitted size in discrete steps, jump around with the outline (table of contents) or a go-to-page prompt, follow annotated links, use Polaris-style dark mode for dark-on-light PDFs, and copy the current page's text to the clipboard (over SSH, via OSC 52). Dark mode uses the selected theme's document colors, preserves document hues, and leaves embedded images unchanged. The status line shows one total render time by default; press `p` to expand it into rendering, dark-mode conversion, compression, and transfer timings.

## Requirements

- Rust 1.88 or newer
- Kitty 0.20 or newer
- macOS arm64, Linux x86_64, or Linux aarch64

Native rendering and editor integration are verified on macOS. Linux runtime
integration is not yet tested.

tmux support is not included yet. Run `pdfterm` directly under SSH until Kitty graphics passthrough is added.

## Install

```console
cargo install --locked --git https://github.com/DavidLapous/pdfterm.git
```

The build downloads PDFium revision 7881 for the target platform, verifies its SHA-256 checksum, and embeds it in the executable. On first use, `pdfterm` extracts the library to `$XDG_CACHE_HOME/pdfterm-private` or `~/.cache/pdfterm-private`. Older native-library caches are not reused.

To build from source:

```console
git clone https://github.com/DavidLapous/pdfterm.git
cargo build --release --locked --manifest-path pdfterm/Cargo.toml
```

## Run

```console
pdfterm
pdfterm document.pdf
```

Use `--pdfium-library PATH` to override the embedded PDFium library, and `--page N` to open at a specific page.

`pdfterm --screenshot /absolute/path.png [--session NAME]` asks the
already-running viewer to save its rendered viewport as a PNG for agent visual
checks. The absolute output path must not already exist; `--session` selects
which viewer to capture.
The PNG contains the currently visible rendered PDF page crops and viewer
highlights/labels, but not terminal text, status rows, or terminal fonts. It
captures only pages already visible in the viewer, not the full document or a
terminal/compositor screenshot; the viewer must be running with its forward
socket available. Requests fail while a search/link picker is open or a newly
selected render is still pending; retry after the current PDF frame appears.

### Keys

| Key | Action |
| --- | --- |
| `j` / down / space / `PageDown` | scroll down continuously across pages |
| `k` / up / `Backspace` / `PageUp` | scroll up continuously across pages |
| `h` / `l` / left / right | scroll horizontally, or change page at the edge |
| Mouse wheel / trackpad | scroll the document, or move through entries in the outline |
| `g` / `G` | first / last page |
| `:` | go-to-page prompt (type a number, `Enter` to jump, `Esc` to cancel) |
| `/` | search selectable document text |
| `n` / `N` | next / previous page containing a search match |
| `m` | cycle fit mode: fit-page → fit-width → fit-height |
| `+` / `-` | zoom in / out in 25% steps (up to 400%) |
| `0` | reset zoom to the fitted size |
| `i` | toggle Polaris-style dark mode |
| `S` | toggle smooth scrolling for the current session |
| `x` | find visible PDF text and jump to its source with SyncTeX |
| `Alt`/`Option` + click | resolve the clicked location via SyncTeX and jump to its source; no toggle |
| `p` | toggle detailed render-performance timings |
| `t` | outline / table of contents (fuzzy filter, `Enter` to jump) |
| `T` | choose and preview a theme for the current session |
| `y` | copy the current page's text to the clipboard |
| `Enter` | open the document-wide link browser |
| `L` | toggle annotation highlights and open the link browser |
| `b` | return to the view before the last followed internal link |
| `f` | open a PDF in a new tab |
| `Tab` / `Shift-Tab` | switch tabs |
| `Alt-1` … `Alt-9` | select a numbered tab directly |
| `?` | open the keybinding help menu |
| `q` | leave link mode when active; otherwise close the current tab |
| `Esc` | leave link mode; otherwise close a pane, clear search, or exit |

Vertical scrolling shows adjacent pages together, separated by one terminal row,
in every fit mode. Links and inverse search target the page under the pointer,
not just the first visible page. The `h`/`l` keys and left/right arrows scroll
horizontally when the rendered page is wider than the viewport.

Search scans and caches selectable text incrementally without blocking foreground
page rendering. Results open beside the PDF, grouped by outline section and page,
with one surrounding-text snippet per matching page. Use `j`/`k`, arrows,
`Ctrl-b`/`Ctrl-f`, or `g`/`G` to navigate, and `Enter` to jump while keeping the
results visible. In a split layout, `h` focuses the PDF and `l` returns to the
results (`Tab` toggles focus); `/` starts a new search and `Esc` closes the pane.
The search is case-insensitive, treats runs of whitespace as a single space, and
highlights matches using the active theme. Image-only PDFs require OCR and are
reported as having no matches.

Press `x` to find text only on the currently visible page or pages; type a query
to update matching highlights and labels as you go, then type a displayed label
to run inverse SyncTeX and deliver the resulting source location to the configured
editor. `Esc` exits; `Backspace` removes label input first, then edits the query.
Labels avoid characters that would extend a current match. An exact label takes
precedence over another query character; once only one match remains, its label
is the next ASCII word character when available, or a regular label otherwise.
When one-key labels cannot identify every match, labels use short multi-key
sequences so every highlighted location remains selectable.

Match highlights use translucent blue so PDF text stays legible; labels use the
magenta and pale text of the current Neovim Flash palette.
Badge glyphs scale with the matched text height (minimum 8 pixels for
readability), use antialiased glyphs from `viewer.flash_label_font` (default
`monospace`), and sit beside the final visible glyph. Set an installed family
name such as `Menlo` to choose another face; `monospace` maps to Menlo on macOS
and the installed generic monospace elsewhere. Unknown or unsupported fonts fail.

This is a viewport search, not a document-wide scan: arrows and PageUp/PageDown
scroll; `Ctrl-+` / `Ctrl--` zoom without leaving the mode; resizing, switching
tabs, or reloading updates the visible matches. It requires selectable PDF text
and a working SyncTeX/editor setup; it does not guess a source location when
SyncTeX cannot resolve the match.
PDF text extraction excludes invisible and transparent glyphs but cannot detect
glyphs covered by later opaque drawing. Like `/` and text copy, `x` is not a
redaction check: sanitize PDFs before using it on sensitive covered content.

Click a PDF hyperlink to follow it; no mode toggle is required. Mouse capture stays
enabled while the viewer runs, including outside `L` mode. Use `y` to copy
the page's text, or your terminal's mouse-capture override for terminal selection.
Press `L` to toggle annotation highlights and open the document-wide link browser.
Press `Enter` to open the same browser without enabling highlights. It uses a
Grimoire-style split view. Wide terminals place
the PDF on the left and a compact link sidebar on the right; narrow terminals
place the PDF above the links. Links are indexed incrementally behind foreground
page rendering and grouped by document section and source page when the PDF has
an outline, with source-page-only headings as a fallback. The
split repositions Kitty's retained page image without rerendering or retransmitting it.
In the browser, use `j`/`k`, arrows, or type an entry number to preview that
link's source location and highlight its annotation in the PDF; rapid selection changes are debounced to
avoid redundant rendering. Click a row to select it, or press `Enter` to follow
the selected link. Closing the browser restores the view from before the preview. `Ctrl-f`/`Ctrl-b`
move by a viewport and `g`/`G` select the first or last link. In a split layout,
press `h` to focus the PDF and `l` to return to the link list (`Tab` toggles focus
too); PDF movement keys then navigate the preview without closing the browser.
While link mode is active, press `Esc` or `q` once to close the browser and
disable link mode. Floating layout remains modal. Press `/` to filter
by label, source page, destination page, or URL;
when the browser was opened with `Enter`, `Esc` clears the filter before closing
the picker. Links are ordered by source page,
then top-to-bottom and left-to-right. Wrapped or column-interleaved PDF annotations
that share a destination are reconstructed as one readable entry. The selected-link
panel shows source and destination details plus, when PDF
text extraction permits it, the surrounding citing text and matching numbered reference.
Context is cached during background link indexing, so browser navigation does not
perform additional PDF work. Link history is kept independently for each tab.
External URLs are copied to the local clipboard with OSC 52 instead of being
opened on the remote machine. Set `persistent_link_picker = true` to keep the
split open after following or copying a link; `Esc` closes it. Press `s` to cycle
vertical, horizontal, and floating layouts, or `a` to restore automatic layout
selection. These runtime layout choices are session-only and do not rewrite the
configuration file. Press `L` to leave link mode. Plain citation text without a
PDF link annotation is not inferred.

## Configuration

On first launch, `pdfterm` creates **`~/.config/pdfterm/config.toml`** with
commented defaults. A nonempty `XDG_CONFIG_HOME` replaces `~/.config`. Existing
files are never overwritten. Invalid TOML, unknown keys, invalid animation
ranges, and file-access errors stop startup with the configuration path.

[config.default.toml](config.default.toml) documents every setting:

- Top level: fit, colors, link-browser layout, SyncTeX, and socket paths.
- `[editor]`: inverse-search delivery: `none`, `command`, or `socket`.
- `[viewer]`: continuous/smooth scrolling, adjacent-page prefetch, animation
  interval/easing, forward-search centering, flash duration and label font,
  word matching and source-context radius.

Restart the viewer after editing its configuration. `?` shows controls and the
active configuration path.
Scrolling eases in image pixels, including across page boundaries; it does not
snap to terminal rows. Cached pages use retained Kitty image placements rather
than rerendering or retransmitting their pixels on each tick.
Unchanged canvas and status content is not repainted. Frame commands are buffered
and flushed at the end of each synchronized update. Active animation waits until
the next `viewer.scroll_frame_ms` deadline rather than a fixed input-poll interval;
integer-millisecond timing and terminal scheduling do not guarantee exact 120 Hz.
Smooth scrolling is off by default. Set `smooth_scroll = true` to animate steps.
`S` toggles smooth scrolling for the current viewer session without changing
the config.
`continuous_scroll = false` retains single-page scrolling.

`viewer.prefetch_pages = 5` renders and caches up to five pages before and after
the current page in the background. Set it to `0` to disable speculative
rendering; visible pages still render on demand. Values from `0` through
`4294967295` are accepted. More pages use more memory and background CPU, but
avoid rendering those pages again when you navigate to them after prefetch
finishes. Foreground requests take priority over queued prefetch work; an
already-running PDFium render must finish first. Resize, zoom, color changes,
and PDF reloads can require fresh renders.

Relative socket names resolve under the configuration directory's `run/`
directory, which is created with mode `0700`. Socket files use mode `0600`.
Absolute socket paths require a real, current-user-owned mode-0700 parent;
the old `/tmp/pdfterm-*.sock` settings are rejected.
Existing endpoints are never unlinked on startup. If a process crashes, stop
any process using that socket and explicitly remove the stale socket before
restarting. Normal exits remove only the endpoint the process created.

`link_picker_layout = "vertical"` keeps the PDF on the left and links on the
right; `"horizontal"` keeps the PDF above the links; and `"floating"` places an
opaque centered browser over the full-size PDF. The default `"auto"` chooses a
split from the terminal shape. `link_picker_split_percent` controls the link pane
in split layouts and is ignored by the floating layout. The legacy `invert` key
remains accepted as an alias for `dark_mode`. `theme` is loaded directly, while
`theme_catalog` supplies an explicit `themes = [...]`
array for the picker. pdfterm never scans a theme directory. Both the shared
`[colors]`/`[ui]` schema and pdfterm's legacy complete-palette schema are accepted. Legacy theme
files contain the complete color palette using `#RRGGBB` values:

```toml
bg = "#222436"
bg_dark = "#1e2030"
bg_dark1 = "#191B29"
bg_highlight = "#2f334d"
blue = "#82aaff"
blue0 = "#3e68d7"
blue1 = "#65bcff"
blue2 = "#0db9d7"
blue5 = "#89ddff"
blue6 = "#b4f9f8"
blue7 = "#394b70"
comment = "#636da6"
cyan = "#86e1fc"
dark3 = "#545c7e"
dark5 = "#737aa2"
fg = "#c8d3f5"
fg_dark = "#828bb8"
fg_gutter = "#3b4261"
green = "#c3e88d"
green1 = "#4fd6be"
green2 = "#41a6b5"
magenta = "#c099ff"
magenta2 = "#ff007c"
orange = "#ff966c"
purple = "#fca7ea"
red = "#ff757f"
red1 = "#c53b53"
teal = "#4fd6be"
terminal_black = "#444a73"
yellow = "#ffc777"

[git]
add = "#b8db87"
change = "#7ca1f2"
delete = "#e26a75"
```

If the selected theme is missing or malformed, pdfterm reports it once and uses
its internal fallback palette. Press `T` to preview and apply any
installed theme for the current session; picker changes do not rewrite
`config.toml`.

Dark mode uses `bg_dark` and `fg` for the document background and foreground by
default. Low-contrast dark-blue text inside PDF link annotations is lifted
toward the document foreground, with the theme's `cyan` as a fallback, so links
remain readable. A theme can override the document background and foreground
independently:

```toml
[document]
background = "#1e2030"
foreground = "#c8d3f5"
```

Recently opened documents are tracked in `$XDG_CACHE_HOME/pdfterm/recent`
(or `~/.cache/pdfterm/recent`) and shown with their parent directories for context.
Filtered picker results are labeled `RECENT`, `HERE`, or `SUBDIR` so their source
remains visible after the recent-files heading is replaced by search results. Use
`j`/`k` or arrows to move, `Ctrl-b`/`Ctrl-f` to move by a page, and, before
entering a filter, `g`/`G` to jump to the first or last result. The file, outline,
theme, link, and search-result pickers use the same navigation conventions.
The outline also accepts mouse-wheel and trackpad scrolling, including while
filtering; the selected entry stays visible and `Enter` jumps to it.

## Editor-neutral SyncTeX

`src/synctex.rs` owns source/PDF resolution; `src/editor.rs` delivers a typed
source location through a command or socket. Socket delivery is enabled by default
for the Neovim plugin. To use another editor, replace the `[editor]` section:

```toml
[editor]
transport = "command"
argv = ["code", "--reuse-window", "--goto", "{file}:{line}:{column}"]
```

Commands run as an argument vector, without an implicit shell, expansion, or
reparsing of substituted filenames. SyncTeX and editor delivery run on a separate
bounded navigation worker, not the event loop or PDFium worker. An inverse request
has a ten-second deadline; a new click, reload, tab change, or shutdown cancels it.
Helpers have a 1 MiB output limit per stream; timeout/cancellation kills their
process group. Commands must hand off to an editor, not remain attached to it.
Nonzero exit status is an error. Placeholders: `{file}` is an absolute source path,
`{line}` is one-based, `{column}` is one-based UTF-16, `{column_char}` is
one-based Unicode scalar, `{byte_column}` is zero-based UTF-8, and
`{column_byte}` is one-based UTF-8. Unknown placeholders are rejected.

The default socket configuration also supports custom editor adapters:

```toml
[editor]
transport = "socket"
path = "editor.sock"
```

The adapter listens on the resolved private socket and receives one JSON object
per connection, terminated by EOF:

```json
{"file":"/project/main.tex","line":12,"byte_column":0,"column":1,"column_char":1,"precise":false}
```

`precise = false` means SyncTeX supplied only a line; all columns point to its
start. This transport does not expect a reply. After successful editor delivery,
the viewer also copies `file:line:byte-column` to the clipboard. With
`transport = "none"`, only the clipboard copy is performed. Editor-delivery
failures are reported without copying; the default socket transport requires a
listening editor.

Any editor can forward-search an already-running viewer:

```console
pdfterm /project/main.pdf --forward-search /project/main.tex --line 12 --column 3
```

Source coordinates are one-based; `--column` counts Unicode scalars. To obtain
the request without sending it, substitute `--synctex-view` for
`--forward-search`. The forward socket accepts one JSON object, at most 4096
bytes, followed by a write-half-close:

```json
{"pdf":"/project/main.pdf","revision":{"device":1,"inode":42,"length":12345,"modified_seconds":1700000000,"modified_nanoseconds":0,"changed_seconds":1700000000,"changed_nanoseconds":0},"page":2,"h":72.0,"v":120.0,"width":250.0,"height":12.0}
```

`pdf` must be absolute. The viewer selects its existing tab or opens a new tab
before applying the request; switching projects does not require restarting it.
Use `--synctex-view` to obtain `revision` together with the geometry. It records
the PDF's Unix device/inode, size, and nanosecond modification/change timestamps,
checked before and after SyncTeX resolution. This is local filesystem identity,
not a cryptographic content digest. Geometry is in points, with `h` the left edge
and `v` the bottom edge measured down from the page top.

The resolver also sends optional `word` context, for example
`{"words":["a","navigation","anchor"],"selected":1}` (`selected` is zero-based).
It uses the saved UTF-8 source, bounded to a regular file of at most 2 MiB;
unreadable or unsupported source files fail resolution explicitly. Command names,
comments, and positions outside a literal word supply no word hint.
Hints contain at most seven words of at most 128 UTF-8 bytes each. Letters,
numbers, and attached Unicode combining marks form words.

Within the selected SyncTeX region, the viewer matches complete PDF words using
case/compatibility normalization and neighboring-word context. A unique best match
flashes only that word and positions scrolling using its actual bounds. Ties,
missing words, and unsupported PDF glyph mappings retain the original region,
with an explicit status message rather than guessing the nearest word. This does
not expand TeX macros or select a different page or Beamer overlay. PDF text
extraction errors fail the request explicitly.

The viewer reloads a different revision **before** validating page count, positions
the target, and replies `{"ok":true,"error":null}` only after submitting the
matching rendered frame to the terminal and flushing output. This does not wait
for terminal compositor completion. The highlight lifetime starts at that
submission, not while loading or rendering.

Each connection receives one terminal reply and closes; no status polling or
request IDs are needed. Changed-again PDFs, unreadable documents, out-of-range pages,
invalid requests, supersession, user-input cancellation, and renderer/viewer
failure return `{"ok":false,"error":"..."}`. Repeat SyncTeX resolution after a
revision rejection; do not resend stale coordinates against a newer revision.
The viewer's submission deadline is 30 seconds; the CLI allows 31 seconds for
the final reply. These are failure ceilings, not readiness delays. A disconnected
client is discarded without retaining its pending request.

Successful forward replies also identify the viewer process with a fresh token
(or the plugin-owned split's launch token). When requested, Neovim focuses a
manually attached viewer through a second `{"type":"focus","viewer_token":"..."}`
request to the same socket, after checking that navigation is still current.
The viewer rejects a different process token or a pending newer forward request
and focuses only its own terminal. No terminal ID crosses the forward socket.

Incomplete, malformed, non-finite, unknown-field, and oversized requests are
rejected. Send the complete request and half-close within 100 ms after connecting.

### Neovim plugin (nvim-pdfterm)

Requires Neovim 0.10 or newer. This repository is also a standard Neovim plugin:
its `lua/pdfterm` modules live at the repository root. It integrates Neovim with
pdfterm only; it does not configure other viewers or editors.

With the repository on `runtimepath`, `require("pdfterm").setup()` needs no
arguments for bidirectional SyncTeX. Open a compiled TeX document and run
`:PdfTermForward` to navigate an existing viewer, or `:PdfTermForwardSplit` to
launch a right-hand terminal split if none is running. Option/Alt-click the PDF
to jump back to its source. Neither direction changes terminal focus by default.
This works locally and through the SSH bridge described below.
The PDF needs a matching `.synctex.gz` sidecar and `synctex` must be on `PATH`.
Keybindings and automatic compilation remain optional.

Before using terminal splits, follow [local Kitty/Ghostty setup](#local-terminal-split-setup).
For SSH, configure the [client shell hook](#client-ssh-shell-hook) in
`~/.bashrc` or `~/.zshrc`; plain `ssh` cannot open or focus a client split.

With ordinary SSH, inverse navigation can reposition Neovim's cursor,
but it cannot focus the remote editor terminal. After the jump, manually return
focus to that Neovim terminal; focusing it requires a supported remote terminal
control bridge and is not provided by ordinary SSH.

Existing configuration files are never overwritten. If upgrading from the old
clipboard-only defaults, set `[editor]` to `transport = "socket"` and
`path = "editor.sock"`. Set `[nvim] focus_on_inverse = true` to return to the
source terminal after inverse search. Restart both Neovim and the viewer after
editing the configuration.

With [Lazy.nvim](https://github.com/folke/lazy.nvim):

```lua
{
  "DavidLapous/pdfterm",
  name = "nvim-pdfterm",
  main = "pdfterm",
  lazy = false, -- register PDF interception before initial buffers are read
  opts = {
    executable = "pdfterm", -- install the viewer separately; PATH or absolute path
    open_pdf = true,        -- optional: opening *.pdf launches/selects pdfterm
    keys = {
      forward = "<localleader>pf",
      build = "<localleader>pb",
      main_file = "<localleader>pm",
      compile = "<localleader>pc",
    },
    -- session = "paper",   -- optional explicit pairing; otherwise unique per editor
    -- attach_only = true, -- optional; never launch a viewer automatically
  },
}
```

For a local checkout, replace the repository string with `dir = "/path/to/pdfterm"`.
With another plugin manager, add the repository root to `runtimepath` and call
`require("pdfterm").setup(opts)` with the same options. Remove any old runtime-path
entry pointing at the former `nvim` subdirectory.

`open_pdf` defaults to false. When enabled, the plugin owns PDF buffer interception
and its Enter-to-retry mapping. Disable competing PDF buffer handlers in your
configuration. Personal paths and keybindings stay in your plugin specification;
builds, sessions, navigation, terminal control, and cleanup belong to the plugin.

For an explicit LaTeX command, add `project` inside `opts`:

```lua
project = {
  main = "main.tex",
  cwd = "/project",
  pdf = "build/main.pdf",
  build = { "latexmk", "-lualatex", "-synctex=1", "-outdir=build", "main.tex" },
}
```

Without `executable`, the bundled adapter uses `target/release/pdfterm` beside
the plugin. Configuration loads asynchronously with `--print-config`, allowing
TOML keybindings to become available without blocking startup. Configuration and
listener errors are notifications, not exceptions through the editor's startup.
The inverse listener opens only on the first navigation or viewer-command action.
Each editor gets a unique session unless `session` is explicitly supplied.

#### Local terminal split setup

Local navigation captures its source terminal at invocation, before asynchronous
configuration, builds, or resolution can observe another focused window. Manual
`:PdfTermViewerCommand` also captures the editor terminal at invocation, including
with `attach_only`; it prints its command after capture completes or fails.
Capture failure still permits socket-only attachment but reports unavailable
focus when requested. `:PdfTermForward` never creates a terminal split; if the
viewer is absent it reports the missing viewer. `:PdfTermForwardSplit` can launch
one using the captured identity when `attach_only` is false.
Set `focus_on_forward = true` to focus the exact viewer after its forward frame
is submitted: a plugin-owned split must return its launch token; a manually
started viewer can focus itself via a separate token-checked socket request.
A different viewer answering during split launch never focuses an unknown split.
Set `focus_on_inverse = true` to return to the captured editor terminal after
inverse search. Both options default to false; explicit `setup()` options
override `[nvim]` values in the TOML configuration.
`:PdfTermOpen` does not shift focus, but a split opened there can be focused on a
later forward search. Both terminals launch a right-hand split beside the captured
source, in its existing tab and OS window.
Kitty window control requires remote-control permission. If Neovim runs as an
embedded/headless process without a controlling terminal, `kitten @` also needs
Kitty's remote-control socket. On macOS, add to `kitty.conf` using a private
runtime directory (`$TMPDIR` is mode 0700 by default):

```conf
allow_remote_control yes
listen_on unix:${TMPDIR}/pdfterm-kitty-{kitty_pid}
```

On Linux, use `${XDG_RUNTIME_DIR}` when available, or another user-private
mode-0700 directory; never expose an unrestricted control socket in a shared
directory. `listen_on` cannot be enabled by reloading `kitty.conf`: save work,
fully quit and reopen Kitty, then restart Neovim to inherit `KITTY_LISTEN_ON`.
Without that socket, a detached Neovim process cannot control Kitty via
`/dev/tty`; the adapter reports the missing socket explicitly. A Neovim
process with a controlling Kitty terminal can still use tty remote control
without a socket.
The adapter selects the source tab's `splits` layout; include `splits` if you
restrict Kitty's `enabled_layouts` (the standard defaults already include it).

Ghostty does not use Kitty's socket; local splits use macOS AppleScript control.
Local Ghostty capture and focus share one lazily started JavaScript-for-Automation
worker per Neovim process. It queries the current front terminal for every capture;
terminal identities are never cached. Requests have a three-second deadline.
A timeout or broken protocol stops the worker and fails its pending requests
explicitly; restart Neovim to retry terminal control. Normal editor exit stops the
worker. Viewer launch/close retain one-shot helpers. Kitty captures its native
window ID without a subprocess and uses the compiled `kitten` client for control.
Its backend lives in `lua/pdfterm/kitty.lua`; `terminal.lua` owns shared launch
and window-ownership policy.
Plain SSH sessions do not control client windows just because terminal identifiers
were forwarded.

#### Client SSH shell hook

Local `nvim paper.tex` needs no SSH helper. To keep automatic splits after
`ssh HOST`, add this to the **client's** `~/.bashrc` (Bash) or `~/.zshrc` (Zsh),
not the remote machine:

```sh
export PATH="/path/to/pdfterm/scripts:$PATH"
export PDFTERM_SSH_HOSTS="workstation other-host"
source /path/to/pdfterm/scripts/pdfterm-shell.sh
```

If your Bash login shell does not load `~/.bashrc`, source it from
`~/.bash_profile`. Start a new local shell after editing.

Then use ordinary commands:

```sh
ssh workstation
cd project
nvim paper.tex
```

The hook intercepts only a bare `ssh HOST` for an explicitly listed alias, in an
interactive terminal outside SSH/tmux. Commands such as `ssh HOST command`,
`ssh -N HOST`, and `ssh -F config HOST` go unchanged to OpenSSH. `scp`, `sftp`,
and Git's SSH subprocesses are unaffected. Use `command ssh HOST` to bypass the
hook. Nothing edits your shell or SSH configuration automatically.

The launcher can also be used directly:

```sh
pdfterm-ssh HOST                 # normal remote login shell
pdfterm-ssh HOST paper.tex       # start remote Neovim directly
pdfterm-ssh -F ./ssh-config HOST # shell using an alternate SSH configuration
```

The client needs Python 3.11+, OpenSSH, and Kitty with remote control enabled, or
Ghostty on macOS with AppleScript control available. Run outside tmux and outside
an existing SSH session. The remote needs Neovim, the pdfterm adapter, the viewer
binary, and TeX tools; its login shell supplies the editor's `PATH`. SSH host
aliases, users, ports, and proxies come from the SSH configuration.

The wrapper opens a remote login shell (or Neovim when arguments follow the host)
and allocates a reverse TCP forward bound to the remote loopback address. It
connects to a private Unix bridge on the client. A fresh 256-bit token, stored
in a remote mode-0600 file inside a mode-0700 directory, authenticates each
terminal-control request; the shell exports the bridge address and token-file
path for Neovim. The remote needs `ss` on Linux or `/usr/sbin/netstat` on macOS
to verify that the listener is loopback-only before the editor starts.
The private SSH master shares the interactive session's foreground process group
and does not request extra confirmation for each multiplexed forwarding request.
Finite control/bootstrap helpers never pass the terminal's input descriptor
to the background master.
On `:PdfTermForwardSplit` or explicit `:PdfTermOpen` when no viewer is running,
the adapter asks the client helper to open a viewer running **SSH back to the same
host**, with the same PDF, session, `PATH`, and configuration directory. Both
Kitty and Ghostty split the original source terminal to the right, within the
same tab and OS window. The PDF, SyncTeX data, and editor/viewer sockets stay
remote; Kitty graphics travel over the viewer's SSH connection. Ordinary
`:PdfTermForward` attaches to an existing viewer without creating a terminal.
Inverse-focus, if enabled, returns to the original client source terminal.

The helper lives for that SSH connection, across successive editor sessions.
Exiting Neovim closes its viewers but leaves the remote shell usable. Shell exit, hangup, and
termination close its owned viewer windows and SSH master; cleanup errors are
reported. SIGKILL or a client crash cannot guarantee cleanup. At most four control
requests are admitted at once, each with a five-second queue/operation budget;
Neovim allows six seconds for transport and reply. A launched viewer remains
provisional until Neovim sends an ownership receipt within the remaining operation
budget. Success is reported only after the bridge confirms that receipt. Missing
receipts roll back the launch; failed confirmation or an ownership callback error
closes the offered handle. Exit waits allow 8.5 seconds, including a separate
two-second cleanup request. Cleanup failures are reported and the bridge retains
ownership. Update the client wrapper and remote adapter together, then restart
the SSH session.

If an existing session reports `missing PDFTERM_LAUNCH_TOKEN_FILE`, its
terminal-control channel lacks matching credentials. Source navigation can
still work, but client-terminal focus cannot; restart through the current
wrapper rather than setting a token path by hand.

Requests and helper output are bounded. The SSH server must permit reverse TCP
forwarding on loopback; refusal, wildcard binding, or unavailable listener
inspection fails explicitly. Other users on a shared remote host can connect to
the loopback port but cannot issue terminal actions without the token; repeated
unauthenticated connections can still delay legitimate requests. There is no
persistent daemon or automatic change to SSH configuration. `attach_only = true`
still forbids launch.

For manual pairing in an ordinary SSH session, keep Neovim and the viewer on the
same remote machine. In Neovim, use:

```vim
:PdfTermViewerCommand
```

This activates the inverse listener, prints a shell-quoted command with the
matching session and PDF, and copies it to the `+` register. With a configured
clipboard provider such as OSC 52 over SSH, the command reaches the client
clipboard. Run it in a second SSH terminal connected to the same machine. Use
`:PdfTermViewerCommand /path/to/document.pdf` to select a PDF
directly. Activate this command **before** inverse-clicking a separately started
viewer; no forward search is required. The local terminal must support Kitty
graphics over SSH. This manual mode needs no client helper or socket forwarding.

With `focus_on_forward = true`, the printed command uses the explicit
`scripts/pdfterm-viewer` launcher. Run it in the foreground terminal that will
display the PDF. On local Ghostty it needs Python 3 to capture that terminal's
AppleScript UUID with a three-second deadline before starting the viewer;
capture failure stops the launch. Direct `pdfterm` invocation cannot recover that
UUID with installed Ghostty 1.3.1. Kitty uses its own `KITTY_WINDOW_ID` and
requires remote-control permission. A viewer in a second wrapped SSH shell
asks **that shell's own** authenticated bridge to focus its source terminal.
Plain SSH still navigates but cannot focus either client terminal; forwarded
terminal environment variables alone never authorize focus. A missing handle,
bridge token, or exact viewer reply fails explicitly.

Public actions are `open(pdf)`, `forward()`, `forward_split()`, `build()`,
`set_main(file)`, and `toggle_compile()`. `open(pdf)` opens or selects a PDF at
page one using the same local/SSH session; it needs neither TeX sources nor a
SyncTeX sidecar. If `forward()` or `forward_split()` cannot resolve a SyncTeX
location, it warns and navigates to page one without source positioning.
A failed compile-before-forward build still stops navigation rather than opening
stale output.
Commands are `:PdfTermOpen [pdf]`, `:PdfTermForward`, `:PdfTermForwardSplit`,
`:PdfTermBuild`, `:PdfTermMain [file]`, and
`:PdfTermCompile`. `:PdfTermViewerCommand [pdf]` / `viewer_command(pdf)` print and
copy the paired viewer invocation. `forward_search(pdf, json_payload)` sends an
already-resolved request to an existing viewer without launching a terminal.
Builds, configuration, and resolution are asynchronous. Navigation generations start
at invocation; stale completions cannot navigate. Builds sharing a canonical
working directory or an output PDF run serially, retaining only the newest
pending build.
Builds time out after 120 seconds; captured build/resolution output is capped
at 1 MiB. Resolution failures are reported and open without source positioning;
build failures stop navigation. Neither path reuses stale coordinates.
Build notifications show the last five output lines (at most 2,000 bytes), updating
at most every 100 ms while compiling. They finish with `Compilation OK` or
`Compilation failed`, retaining the log tail for five seconds. A notification
provider supporting notification IDs, such as Snacks, updates the same popup
instead of appending a separate message on each refresh.

`project.build` is an argument vector, not a shell command string. It runs in
`project.cwd` with the same serialized queue, progress reporting, output bound,
and timeout as the default LaTeX build. Use an explicit shell invocation only when
shell syntax is required. The configured command must produce `project.pdf` and,
for source navigation, its SyncTeX sidecar. This plugin currently supports LaTeX;
Typst and other generators are not implemented.

Without a project descriptor, the selected/current TeX file, its directory,
adjacent PDF, and `latexmk -pdf -interaction=nonstopmode -synctex=1` are used.

No editor keybindings are installed by default. Set `opts.keys` in your plugin
specification or `[nvim.keys]` in TOML: `forward` saves and forward-searches,
`build` builds the main TeX file in TeX buffers, `main_file` selects the current
TeX file as main, and `compile` toggles compilation before forward search.
Empty or omitted keys remain unmapped. Explicit Lua keys are available immediately,
including when the executable or TOML is broken; actions report the failure.
TOML-only mappings appear after background configuration finishes. `opts.keys`
replaces the complete TOML key table rather than merging individual bindings.
Restart Neovim after changing the configuration.

### Forward-search precision

Forward navigation dismisses open Help, file, outline, and theme menus before
positioning the document. The reply still waits for the highlighted frame.

Forward search currently uses the first complete SyncTeX result. For Beamer
overlays (`\pause`, `\only`, `\uncover`, `\visible`), this may select a page where
the target is hidden. Collected frame bodies can also make SyncTeX return a
neighboring frame rather than the target frame. Correct overlay selection is
not guaranteed; always selecting the last result would not fix `\only`.

### Inverse-search precision

`Alt`/`Option`-click resolves the clicked location with `synctex edit`, then
matches the clicked PDF word or mathematical atom against source lines within
`viewer.source_context_lines` of the result (default four).
Inside a literal `\begin{frame}` … `\end{frame}` block or `\caption{...}` argument,
it searches that complete scope instead: collected frame/caption bodies can map
all their contents to the closing line. Nearby PDF words disambiguate repeated
source words; equally good matches within the scope remain line only rather than
favoring the occurrence nearest its end. For inline math, immediately adjacent
literal prose also helps distinguish repeated expressions.

Mathematical matching recognizes literal `$...$`, `$$...$$`, `\(...\)`, `\[...\]`,
and common equation environments. It matches supported TeX commands to Unicode
symbols and normalizes mathematical alphabet styles without lowercasing variables.
Commands retain their source position at the backslash; literal letters retain
their own position. Supplementary Unicode characters remain intact even when
PDFium exposes their UTF-16 halves separately.

This is lexical matching, not a TeX macro expander. Unsupported expressions,
ambiguous matches, and font-private glyphs remain **line only**; there is no
nearest-word fallback. Punctuation needs matching neighboring context. Unknown
macro expansion and expressions with multiple scripts block mathematical
precision throughout the candidate scope: their rendered glyphs or extraction
order cannot be established lexically. A single supported script can still use
literal inline context. Native PDF glyph hit-testing can select an adjacent
glyph when characters are small or overlap; refinement uses the glyph selected.

Each displayed frame records the PDF and companion SyncTeX filesystem revision.
Clicks carry that revision through hit-testing and check it before resolution
and again before editor delivery. Replaced files invalidate the click; repeat it
after reload. The watcher follows companion-only replacements as well.
Metadata equality is not proof that PDF and SyncTeX came from the same build:
publish the completed pair together and avoid concurrent builds of one output.

PDF text extraction and source refinement are optional. Read/encoding errors or
sources larger than 2 MiB preserve the valid SyncTeX line with an explicit warning.
Failed SyncTeX resolution and editor delivery remain errors.

Source matching uses the saved file, so save and rebuild after edits.
With `transport = "none"`, the target is shown in the status bar and copied to the
clipboard (OSC 52). Configured transport failures are reported.

Each session supports one viewer socket and one editor adapter socket, with
multiple document tabs. `pdfterm --session paper ...` and
`setup({ session = "paper" })` select distinct endpoints while sharing config.
Names contain 1–24 ASCII letters, digits, `_`, or `-`; socket path limits still
apply. The standalone CLI's unnamed session retains existing endpoint names;
the Neovim adapter defaults to an automatically generated session instead.
A second editor using the same explicit session fails only its navigation
action, with a live-listener or stale-socket diagnostic. It never steals or
automatically removes an endpoint. Stop its owner before removing a stale socket.

## Security and licensing

Only open trusted PDFs and TeX projects. PDFium parses documents inside the
viewer process; it is not sandboxed. Editor integration can open files and
trigger the configured editor's file-opening hooks. Private socket directories
exclude other OS users, not malicious processes running as your own account.

The embedded native library is checked against its full embedded contents before
reuse. Unsafe cache directories, symlinks, or modified libraries are rejected,
not silently repaired. `--pdfium-library` explicitly loads code you supply.
After investigating a rejected native cache, stop the viewer and explicitly
remove the affected `pdfterm-private` cache directory before retrying.

Source licensing follows the upstream `license = "MIT"` declaration in
`Cargo.toml`; [LICENSE](LICENSE) supplies the MIT text and author attribution.
PDFium and its dependencies retain their own licenses. Run
`pdfterm --third-party-licenses` to print the notices supplied with the pinned,
checksum-verified PDFium archive. Include that output with redistributed
binaries; it does not replace license obligations for other bundled dependencies.

## Checks

```console
scripts/check-all
```

The checks format, typecheck, lint with warnings denied, test, and build the release binary. Protocol tests use generated byte buffers and never use real PDF content.

Terminal backends share explicit operation contracts and the same native parity
probe. With the relevant terminal running and automation authorized:

```console
python3 tests/terminal_native.py --terminal kitty --to unix:/path/to/kitty-control.sock
python3 tests/terminal_native.py --terminal ghostty
```

The probe creates and removes its own surfaces. It starts local Neovim in a
new session without a controlling terminal; Kitty's socket must still launch
and focus the exact right split. It also checks that removing the socket gives
an actionable error without changing Kitty surfaces. Both backends check
client-side SSH control, quoted arguments, environment, liveness and repeated
cleanup. It does not connect to an SSH host or verify rendered PDF pixels.
