# Current review

Scope: SyncTeX precision, pointer controls, continuous scrolling, and centered forward search.
Local review only; delegated reviewers and review servers were disabled at the user's request.

- Option-click resolves the actual visible page, matches nearby PDF/source words,
  and sends a zero-based UTF-8 byte column. A native second-page click resolved
  `paper.tex:11:30` (`bananas`); the real Neovim listener moved to that exact cursor.
- The source matcher covers repeated words, Unicode byte columns, ligatures,
  ambiguity, and the four-line boundary near the beginning of a file. Ambiguous
  prose and unexpanded TeX remain explicitly marked line-only; saved source is required.
- Decoded native Ghostty graphics show adjacent pages separated by one terminal
  row. Twenty-four seeded navigation/fit actions produced 32 positive-sized
  placements. Earlier wheel testing produced 63 in-bounds crops from 16 deltas.
- Forward search centers the box across page boundaries. A native cross-page
  measurement placed its center at 569.43px in a 1140px viewport (0.57px error).
  Eight further requests produced 14 in-bounds image crops; interior highlights
  landed within one terminal row of center and document endpoints clamped.
  Replaced and expired highlights disappeared, including on the second visible page.
  Local review found and fixed cached highlights surviving newer requests.
- The release build and 101 tests pass, excluding the known baseline picker-label
  failure recorded in next_steps.md. The help-wording test was removed rather than
  repinned. Full-close forward-socket coverage remains in place for macOS EINVAL.
- Native sampling (2 seconds, 184 samples) mostly showed event/semaphore waits;
  no performance improvement is claimed. Desktop capture is unavailable, so visual
  confirmation used decoded terminal graphics.
- Existing limits remain: global single-document sockets, row-granular scrolling,
  and prose matching rather than TeX expansion. Neovim supplies PATH to Ghostty
  so the launched viewer can find SyncTeX.
