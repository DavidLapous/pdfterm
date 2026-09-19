# pdfterm

`pdfterm` is a low-latency PDF viewer for Kitty terminals. It renders on the machine where the command runs, compresses each page once, and sends the bitmap through Kitty's graphics protocol. Direct SSH sessions need no local helper.

![pdfterm rendering an arXiv paper in dark mode inside Kitty](assets/pdfterm-dark-mode-arxiv.png)

The current viewer fits one page to the terminal, keeps the current and adjacent pages in memory, and gives foreground renders priority over prefetch work. It reloads each open document automatically when the PDF changes while preserving that tab's current page. Run it without a path or press `f` to open a fuzzy PDF picker in a new tab; recently opened documents appear at the top and remain searchable alongside recursively discovered PDFs. Picker searches match filenames and parent directories, with filename matches ranked first. Press `/` to filter a picker; `Esc` clears an active query before closing it.

You can fit pages to the terminal width or height and scroll through the overflow, zoom in and out beyond the fitted size in discrete steps, jump around with the outline (table of contents) or a go-to-page prompt, follow annotated links, use Polaris-style dark mode for dark-on-light PDFs, and copy the current page's text to the clipboard (over SSH, via OSC 52). Dark mode uses the selected theme's document colors, preserves document hues, and leaves embedded images unchanged. The status line shows one total render time by default; press `p` to expand it into rendering, dark-mode conversion, compression, and transfer timings.

## Requirements

- Rust 1.85 or newer
- Kitty 0.20 or newer
- macOS arm64, Linux x86_64, or Linux aarch64

tmux support is not included yet. Run `pdfterm` directly under SSH until Kitty graphics passthrough is added.

## Install

```console
cargo install --git https://github.com/jrf/pdfterm.git
```

The build downloads PDFium revision 7881 for the target platform, verifies its SHA-256 checksum, and embeds it in the executable. On first use, `pdfterm` extracts the library to `$XDG_CACHE_HOME/pdfterm` or `~/.cache/pdfterm`.

To build a checkout instead:

```console
cargo build --release
```

## Run

```console
pdfterm
pdfterm document.pdf
```

Use `--pdfium-library PATH` to override the embedded PDFium library, and `--page N` to open at a specific page.

### Keys

| Key | Action |
| --- | --- |
| `j` / down / space / `PageDown` | scroll down continuously across pages |
| `k` / up / `Backspace` / `PageUp` | scroll up continuously across pages |
| `h` / `l` / left / right | scroll horizontally, or change page at the edge |
| Mouse wheel / trackpad | scroll continuously (outside pickers and prompts) |
| `g` / `G` | first / last page |
| `:` | go-to-page prompt (type a number, `Enter` to jump, `Esc` to cancel) |
| `/` | search selectable document text |
| `n` / `N` | next / previous page containing a search match |
| `m` | cycle fit mode: fit-page → fit-width → fit-height |
| `+` / `-` | zoom in / out in 25% steps (up to 400%) |
| `0` | reset zoom to the fitted size |
| `i` | toggle Polaris-style dark mode |
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
- `[viewer]`: continuous/smooth scrolling, animation interval and easing,
  small/page scroll distances, filename titles, forward-search centering and
  flash duration, word matching and source-context radius.
- `[nvim]`: initial viewer, compile-before-search, inverse-search focus, and
  executable override. Empty `executable` uses this checkout's release binary.
- `[nvim.keys]`: forward search, build, main file, compile toggle, and viewer
  selection. An empty binding disables that mapping.

The bundled Neovim plugin reads this same file through `pdfterm --print-config`;
there is no second configuration to synchronize. Restart the viewer and Neovim
after editing it. `?` shows controls and the active configuration path.
Scrolling eases over terminal rows; `smooth_scroll = false` restores immediate
steps. `continuous_scroll = false` retains single-page scrolling.

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

## Neovim integration

Build the release binary, then add the bundled plugin to Neovim:

```lua
vim.opt.runtimepath:prepend(vim.fn.expand('~/git/pdfterm/nvim'))
require('pdfterm').setup()
```

No modules from a separate Neovim configuration are required. The plugin owns
its keymaps, compilation, forward search, inverse-search socket, and source-window
focus. It supports **Kitty and Ghostty**; unsupported terminals fail explicitly.
Kitty needs `kitten` on `PATH` and remote control permitted (for example, a
`listen_on` Unix socket with `allow_remote_control socket-only`). Use Kitty's
`splits` layout for a right split. Ghostty needs its AppleScript interface enabled.
No terminal settings are modified by the plugin.

With `nvim_socket` set, `Alt`/`Option`-click resolves the clicked location with
`synctex edit`, then matches the clicked PDF word and nearby text against source
lines within `viewer.source_context_lines` of the result (default four).
The handoff is `file:line:byte-column` (one-based line, zero-based UTF-8 byte
column). The plugin jumps to that source buffer and focuses the exact terminal
captured by forward search, unless `nvim.focus_on_inverse = false`.
Ambiguous words, macros, and non-text clicks remain explicitly marked **line only**;
the matcher does not expand TeX. Source matching uses the saved file, so save and
rebuild after edits. Without the socket, the target is shown in the status bar and
copied to the clipboard (OSC 52). Configured editor-socket failures are reported.

The nvim forward search (`<leader>cl`) uses a switchable viewer:

- `<leader>csls` — Skim (displayline)
- `<leader>cslt` — pdfterm beside the source Kitty or Ghostty terminal (default)

The terminal branch runs `synctex view` for the cursor line and character column,
uses the first (best-ranked) result, and sends
`page:h:v:W:H` (1-based page, h = box left, v = box bottom measured from the
page top) to the configured `forward_socket`. A live viewer applies the goto,
flashes the target box red for one second, and centers it vertically, showing
adjacent pages as needed. Centering and flash duration are configurable.
Positioning is rounded to terminal rows and clamped at document ends.
With no viewer listening, the plugin launches pdfterm beside the captured source
terminal and sends the payload once its socket is up. The split inherits `PATH`
and `XDG_CONFIG_HOME`. The viewer sets its terminal title to the PDF filename
unless `viewer.set_window_title = false`.

Default editor controls: `<leader>cl` forward search, `<leader>cb` build,
`<leader>csl` set main TeX file, `<leader>cscl` toggle compile-before-search.
Socket paths are global by default: one active document/editor pair per socket
pair. Use separate configurations/socket paths for independent sessions.

## Checks

```console
scripts/check-all
```

The checks format, typecheck, lint with warnings denied, test, and build the release binary. Protocol tests use generated byte buffers and never use real PDF content.
