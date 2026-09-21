# pdfterm review

Updated: 2026-09-21 PDT.
Reviewed integration baseline: `d2a99eb42ed8c59d6be0b3635c39be59fffa171a`.
Publication source: `23286f232b966623a1e9eadcda9e6f090071b222` (`main`).
The intervening standalone-PDF commit was inspected: it adds `open(pdf)` and a
headless regression, but does not change the launcher or repair the findings below.
This is a focused editor/terminal/SSH review, not a new full rendering audit.
This publication changes documentation only; no finding is closed by writing it down.

Implementation instructions: [next_steps.md](next_steps.md).
Required behavior: [invariants/editor-session-lifecycle.md](invariants/editor-session-lifecycle.md).
The existing [TODO.md](TODO.md) remains the separate viewer-feature backlog; this
review neither implements nor revalidates those features.

## Assessment

Keep the present module boundaries. The Rust navigation worker, Lua socket/project/
terminal modules, and client-side SSH launcher have distinct useful responsibilities.
The next work is a focused identity/lifecycle hardening pass, not another rewrite or
an additional persistent session manager.

The implementation now contains asynchronous configuration/build/resolution,
intent generations, serialized builds within an adapter instance, private named
socket sessions, and separation of socket attachment from terminal launch/focus.
`src/navigation.rs` checks document revisions before resolution and again before
inverse delivery; PDF-text refinement failure is reported while retaining a real
line-level SyncTeX result. These are implementation observations, not a claim that
the complete acceptance suite was rerun for this review.

## Open findings

### P1 PDFTERM-LIVE-WINDOWS — historical IDs exhaust the live-viewer limit

Owner: `scripts/pdfterm-ssh`, `Bridge.dispatch`, `Bridge.owned`, and the terminal
backend's liveness/close operations. Coordinate with `nvim/lua/pdfterm/init.lua`
(`owned_splits` and editor shutdown).

A launch inserts the terminal ID in `owned`. Only a bridge `close` request removes
it. Quitting pdfterm or closing its terminal directly leaves that ID counted.
The next launch rejects when `len(owned) >= 16`, even when no viewer remains live.
A copied-source mock-backend reproduction launched and externally closed 16 unique
viewers; launch 17 failed with `too many owned viewer windows`, with zero live
windows and 16 retained IDs. A long-running editor can reach this limit; normal
editor shutdown is a separate cleanup path and is not the reproduction.

Count confirmed live, owned viewers, reconcile external exits, and remove obsolete
ownership bookkeeping. A failed liveness query is not evidence that a window is
absent. Raising the cap or clearing all ownership on error is not a fix. Preserve
protection of the source terminal and independently launched viewers.

### P1 PDFTERM-SOURCE-IDENTITY — Ghostty is queried after the initiating work

Owners: `nvim/lua/pdfterm/init.lua` (`forward`, `deliver`, `launch`, now also `open`)
and `nvim/lua/pdfterm/terminal.lua` (`capture_source`).

On the missing-viewer path, terminal capture occurs after configuration, optional
build, and SyncTeX resolution. The Ghostty branch queries the focused terminal of
the front window. If navigation began in A and focus moved to B during compilation,
the returned handle is B: the split and subsequent inverse-focus target can be
wrong. This is a source/control-flow finding, not a reproduced macOS GUI test.
Kitty's inherited window ID and the client bridge's already-captured source are
different mechanisms; do not describe them as the same focus-query defect.

Associate the source handle with the initiating intent before lengthy asynchronous
work; retain it rather than rediscovering focus during completion. A failure to
capture must not prevent socket-only attachment. A terminal side effect without a
valid original target must fail explicitly rather than selecting the current window.
The public `forward_search(..., source)` supplied handle must remain authoritative.

### P2 PDFTERM-HELPER-TREE — timeout/output cleanup only kills the direct child

Owner: `scripts/pdfterm-ssh`, `run`, its callers, and `main`'s SSH-master lifecycle.

`run` bounds output and elapsed time, but its `finally` block kills/reaps only the
immediate `Popen` child. A helper's descendants can continue. In the copied-source
reproduction, a helper timed out at approximately 0.50 seconds and its descendant
subsequently wrote a marker. This establishes direct-child-only cleanup, not an
observed failure of a particular real SSH proxy configuration.

Bounded helpers need explicit descendant ownership. SSH authentication/bootstrap
and the intentionally persistent control master need a separate, explicit lifetime.
Do not blindly detach every invocation from its controlling terminal, kill an
unrelated process group, or kill the successful shared master after each command.
The detailed plan requires tests for both failure cleanup and retained authentication.

## Validation evidence and limits

The prior review's scratch script copied `run` and `Bridge` from `d2a99eb`; those
launcher sources are unchanged in `23286f2`. Re-running that script during this
publication reproduced the live-window accounting defect and descendant survival;
its real-Unix-socket protocol sanity checks passed. The backend was mocked, not
Kitty/Ghostty. Copied implementation is diagnostic evidence, not a maintained oracle:
regression tests must load the repository's actual launcher with `runpy.run_path`.

The prior review also recorded a Bash hook-routing check: selected bare interactive
aliases were intercepted and argument-bearing, explicit-bypass, remote, and
noninteractive invocations passed through. That Bash check was not rerun in this
publication. No zsh, real SSH/authentication, macOS window automation, complete Rust
suite, or headless-Neovim suite was executed here. `cargo`, `rustc`, `nvim`, `luac`,
and `zsh` were unavailable in the publication environment.

The repository has Linux/macOS CI and adapter/launcher tests. The original review
found no workflow run for `d2a99eb`; that historical check is not a result for the
newer head or this documentation commit. Record exact tested SHAs and distinguish
CI, mocks, headless Neovim, and native terminal tests before a reliability sign-off.
