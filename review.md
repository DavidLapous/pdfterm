# Current review

Scope: landscape orientation and macOS forward-socket lifetime. Self-reviewed;
no delegated review, as requested.

- Rendering and inverse search share a fit configuration that preserves document
  orientation. Actual course-PDF Kitty pixels are upright (700x525 instead of
  sideways 525x700); five additional viewport geometries passed.
- Forward reads use nonblocking I/O with a 100ms deadline. This avoids macOS
  `SO_RCVTIMEO` returning EINVAL after the sender closes. The full-close regression
  passes alongside existing EOF/timeout coverage.
- 101 tests passed; the known baseline picker-label failure was excluded. Release
  build passed. Bounded native samples were mostly event waits, not evidence of a
  performance improvement.
- Real Ghostty cold launch and repeated Neovim requests retained the same window,
  split and viewer PID, without Neovim errors.
- Page/flash/scroll acceptance remains unverified: a bare-PTY navigation probe
  stayed alive but emitted no post-request page indicator. See `next_steps.md`.
