# pdfterm next steps

Updated: 2026-09-21 PDT. Source: `23286f232b966623a1e9eadcda9e6f090071b222`.
Findings and evidence limits: [review.md](review.md).
Contract: [invariants/editor-session-lifecycle.md](invariants/editor-session-lifecycle.md).
Only unresolved work is listed. Do not mark tasks complete without the indicated
regressions and an adversarial review. Keep the current architecture and explicit
errors; no compatibility ladders, automatic fallback viewers, or durable launch broker.

## 1. P1 PDFTERM-LIVE-WINDOWS — reconcile actual viewer lifetime

Files: `scripts/pdfterm-ssh` (`Windows`, `Bridge.dispatch`, `Bridge.close`),
`nvim/lua/pdfterm/init.lua` (owned handles), `nvim/lua/pdfterm/ssh.lua`, and
`tests/ssh_launcher.py`.

- [ ] Add a small terminal-backend liveness operation, for example
  `live_ids(identifiers: set[str]) -> set[str]`. Its result must be restricted to
  the supplied owned IDs. Query exact IDs, never titles or the currently focused
  window. Decode successful backend results strictly. A failed helper, malformed
  result, or timeout raises an explicit error; it must not mean an empty live set.
- [ ] Reconcile externally closed viewers before enforcing the existing live-window
  limit. Prefer a batched bounded query to one subprocess per historical launch.
  Remove confirmed dead records so long-lived shells do not accumulate a launch
  history forever. Do not raise the limit or add a polling daemon as the solution.
- [ ] Coordinate this with the Lua adapter's retained `owned_splits`. A later editor
  shutdown must handle previously retired handles deliberately, without reporting
  successful closure of arbitrary unknown IDs. Keep source/unowned-window refusal.
  Document the already-gone outcome and its ownership proof in the same patch;
  do not fix bridge accounting while leaving stale Lua handles as shutdown errors.
- [ ] Preserve graceful close and bounded cleanup. If a known viewer disappears
  between liveness lookup and close, handle that race as an already-gone owned
  viewer, not permission to act on another terminal. If backend close fails while
  the viewer still exists, retain ownership and report the failure.

Required regression design: load the actual `scripts/pdfterm-ssh` via
`runpy.run_path`, as the existing tests do. The fake `Windows` must allocate a NEW
ID for each launch and model external close separately from a bridge close request.
The current constant `viewer` fixture cannot expose historical-ID growth.

Acceptance cases: at least 32 launch/external-close cycles in one live editor;
16 simultaneously live viewers still enforce the limit; mixed live/dead IDs;
query failure retains ownership and rejects explicitly; close between query and
cleanup; editor exit after reconciliation; successive editors in one shell;
malformed requests never control the source/unowned windows. Finally repeat actual
quit/reopen and editor/shell exit in both native terminal backends.

## 2. P1 PDFTERM-SOURCE-IDENTITY — retain the initiating terminal

Files: `nvim/lua/pdfterm/init.lua`, `terminal.lua`, `ssh.lua`, and focused headless
adapter tests. Include the new `open(pdf)` path, not only TeX forward search.

- [ ] Introduce a small request-local source record tied to the existing navigation
  generation, not a second session graph. Carry it through configuration, build,
  resolution, delivery, launch coalescing, and inverse-focus association. An explicit
  public source handle or the SSH bridge source is authoritative.
- [ ] When local launch/focus is enabled, acquire the optional source candidate at
  user initiation before lengthy work. Prefer a stable terminal-provided/injected
  identity where available. A focused-window lookup is not a durable editor ID:
  never defer it until build/resolution completion and claim it identifies the
  initiator. If initial configuration prevents reliable early capture, use a
  validated explicit identity or report the terminal operation unavailable; do not
  guess from late global focus.
- [ ] Keep capture failure separate from transport failure. Sending to an existing
  forward socket must still work without a supported terminal, scripting permission,
  or launch helper. Plain SSH and attach-only with focus disabled must not attempt
  local terminal control. Only a required launch/focus side effect depends on a
  valid target; report its failure without silently retargeting.
- [ ] Remove completion-time rediscovery of the source. Ensure superseded callbacks
  cannot overwrite the latest accepted source handle. If a launch is shared by
  coalesced requests, define which valid initiating target owns it and do not
  transfer an already-started launch to a newly focused surface.
- [ ] Preserve opt-in inverse focus and exact handle-based close. Do not add title
  scraping, a mandatory liveness probe on the socket-only path, or a fallback to
  whichever window is frontmost.

Acceptance: begin in Ghostty A, hold build/resolution, switch focus to B, release:
launch and enabled inverse-focus still target A. Repeat for existing-viewer reuse,
standalone PDF open, two superseding requests, editor exit during capture/launch,
missing terminal identity, and disabled focus. Headless tests should control the
capture/build callbacks and assert retained handles; separately run the native A/B
window test. Keep the existing plain-SSH tests that prohibit local terminal calls.

## 3. P2 PDFTERM-HELPER-TREE — define bounded helper ownership

Files: `scripts/pdfterm-ssh` (`run`, `Windows` helper calls, SSH bootstrap/master
startup and cleanup), `tests/ssh_launcher.py`.

- [ ] Enumerate the actual invocation lifetimes before editing: finite terminal
  helpers, finite SSH control/cleanup commands, the authentication/bootstrap step,
  the shared control master, and the foreground interactive editor/shell connection.
  The bootstrap currently uses the same `run` while intentionally creating a
  persistent master. A blanket kill-all-descendants-on-success change is incorrect.
- [ ] Give finite helpers an explicitly owned child/process group and bounded
  terminate/kill/reap cleanup, covering timeout, output overflow, cancellation,
  startup failure, and descendants holding stdout/stderr open after parent exit.
  Check cleanup errors; do not silently ignore failure or signal the reviewer's/
  user's process group. Prevent a stale PID/group handle from becoming authority
  over a replacement process.
- [ ] Separate persistent SSH-master ownership from finite helper execution with
  the smallest explicit API needed. Preserve controlling-terminal authentication
  during bootstrap and retain a successful master for viewer connections. Clean
  failed bootstrap and owned proxy descendants without treating them as a working
  shared master. Do not blindly set `start_new_session=True` on authentication and
  assume password/host-key prompts still work.
- [ ] Keep existing byte/deadline limits, quoted remote-only argv execution, source
  window protection, and normal shell-exit cleanup. Choose one supported Python
  baseline deliberately if an API change is needed; do not add version fallbacks.

Regression: a real helper spawns a child that writes a delayed marker, then blocks.
Synchronize on a ready marker so the timeout cannot occur before child creation.
After timeout, assert the descendant is gone and no delayed marker appears; always
clean fixtures. Repeat for output overflow and a parent that exits while its child
keeps pipes open. Use actual source, not copied `run` code. Separately test retained
master reuse, failed authentication/bootstrap, a controlled proxy-child fixture,
SIGTERM/SIGHUP, and normal remote shell exit. Test authentication with a PTY where
needed, without printing or persisting credentials.

## 4. Acceptance gate and documentation

- [ ] Keep the maintained launcher tests loading actual repository code; replace
  diagnostic-only copied-source probes with the regressions above. No real account,
  terminal, or SSH access is needed for the unit fixtures.
- [ ] Run formatting/lint/build, the full Rust test suite, headless adapter/session
  tests, and Python launcher tests on the recorded source SHA. The current CI skips
  `picker_labels_recent_files_with_parent_directory`; report that exclusion until
  independently fixed, never present the run as an unqualified full-suite pass.
- [ ] Run Bash AND zsh hook routing; missing zsh is untested coverage, not a pass.
  Check selected bare interactive alias, unmatched host, command/options, explicit
  `command ssh`, nested SSH, tmux, and noninteractive bypass behavior.
- [ ] Run native Kitty/Ghostty checks for A/B focus, viewer manual exit/reopen,
  simultaneous editors, standalone PDF, quoted paths, custom PATH/XDG config,
  inverse-first attachment, and editor/shell/connection shutdown. Verify external
  viewers survive. Test both local use and the client-launched remote-shell route.
- [ ] Record exact commands, exit status, platform and Neovim/terminal versions,
  tested source SHA, and whether evidence is mock, headless, or native. A missing
  run or skipped platform is not a reliability certificate.

After each independently verified repair, remove the corresponding open finding
and task; retain only a concise evidence note. Keep README behavior and the invariant
consistent with actual code. The consumer's keymaps, legacy GUI-viewer setup and
Texlab backend configuration belong to its configuration repository, not this one.
