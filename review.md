# Current review

Scope: editor-neutral SyncTeX, publication hardening, diagonal-scroll regression,
opt-in inverse focus, and owned terminal lifecycle. Review was local only;
delegation and review servers were disabled at the user's request.

## Terminal/platform extraction

- Editor orchestration no longer constructs Kitty commands or AppleScript.
  `terminal.lua` dispatches capture/launch/focus/close through two adapters;
  `platform.lua` owns macOS process operations and explicit OS checks.
- Native isolated Neovim sessions launched and focused PDF splits in Kitty and
  Ghostty. Quitting each editor removed its viewer and both socket endpoints;
  the independent Kitty surface remained. Skim opened the exact fixture PDF.
- A bounded throwaway smoke passed 272 platform/invalid-handle checks, including
  simulated non-macOS rejection before invoking AppleScript or Skim. This is
  branch coverage, not Linux runtime verification.
- A two-second sample of the native Neovim session recorded all 1,772 main-thread
  samples in `kevent`; no performance improvement is claimed.
- No Rust source, graphics encoding, socket protocol, or build target changed.
  The existing macOS/Linux target restriction remains in `build.rs`.

## Retained publication verification

- Release build, 105 tests, and Clippy with warnings denied pass. One independently
  established baseline picker-label failure remains excluded.
- Native Kitty reproduced diagonal scrolling from page 8 back to page 1 before
  the fix. Horizontal wheel input now only pans; keyboard page stepping remains.
  Seeded wheel input and uncached rendering checks stayed on the expected pages.
- Rust owns SyncTeX resolution and typed UTF-8-byte, Unicode-scalar, and UTF-16
  columns. A real Unicode fixture resolved line 5 to byte column 7, UTF-16 column
  5, and scalar column 4; socket JSON and literal command argv delivery passed.
- Native forward requests selected the matching open PDF tab and rejected an
  unopened PDF. All 161 malformed/oversized protocol cases were rejected; a
  subsequent valid request and a fully closed peer did not kill the viewer.
- Twenty-three invalid editor configurations were rejected. Valid command
  configuration loaded, and inverse focus defaulted to false. An actual Neovim
  cursor jump left the active viewer terminal focused.
- Quitting isolated native Neovim closed its owned viewer in both Kitty and
  Ghostty, removed both sockets, and preserved an independent viewer. Ghostty's
  retained exit screen required a further key; teardown handles that explicitly.
  Ctrl-C also exited native help, file-picker, and theme-picker scenarios.
- Socket parents are private and listener cleanup checks inode identity.
  Existing endpoints are not stolen. Cache checks reject symlinks and same-size
  tampering; verified files are reused. No implicit adjacent library is loaded.
- The pinned macOS archive checksum matched. The binary's license output
  preserves all 16 bundled notice files byte-for-byte.
- A three-second sample of the final native release recorded 2,636 of 2,652 main
  thread samples in `kevent`; the worker waited on a semaphore. This is an idle
  observation, not a speedup measurement.
- PDFium remains in-process and unsandboxed. Linux runtime integration is not
  verified. Saved-source refinement does not expand arbitrary TeX macros.
