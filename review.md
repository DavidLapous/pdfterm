# Current review

Scope: smooth continuous scrolling, shared configuration, bundled Neovim plugin,
terminal titles, inverse-search focus, and responsive help.
Local review only; delegated reviewers and review servers remain disabled at the
user's request.

- Ghostty and Kitty launched the viewer from the standalone bundled plugin,
  retained source focus, and displayed `paper.pdf` as the viewer title.
  Ghostty native Option-click returned focus to the exact source terminal.
  Kitty mouse-protocol input resolved `bananas` to source line 9, byte column 30,
  moved the real Neovim cursor, and focused its original Kitty window.
- Scrolling eases over terminal rows, not individual pixels. A settled Kitty
  keypress produced 14 placements with animation, versus two with animation
  disabled. Thirty-two seeded navigation/fit/zoom actions produced 201 positive,
  in-bounds crops. Cached scrolling required no image retransmission.
- Forward highlighting remains centered across adjacent pages. The previous
  boundary/expiry checks remain covered; current native graphics were decoded
  and inspected. Desktop screenshot capture is unavailable.
- The help overlay was checked in a 74-column Kitty pane: stacked binding
  sections and wrapped configuration guidance remained readable.
- Fifty-eight CLI configuration cases exercised numeric boundaries, malformed
  TOML, unknown keys, wrong types, and partial key overrides. Missing files get
  commented defaults; existing files are preserved. The real
  `~/.config/pdfterm/config.toml` was byte-for-byte unchanged.
- Release build, 101 tests, and Clippy with warnings denied pass. The known
  baseline picker-label test remains excluded; see `next_steps.md`.
- A three-second sample during native navigation recorded 2,604 of 2,646 main
  thread samples in `kevent`; the worker waited on a semaphore. Some release
  frames lacked symbols. This does not establish a speedup.
- Remaining limits: global default sockets, saved-source prose matching rather
  than TeX expansion, and source-terminal focus captured by forward search.
  One forward write reported EPIPE while a test editor was blocked by a
  file-change prompt; its cause was not isolated. Normal isolated editor flows
  passed; blocked-editor transport reliability is not established.
