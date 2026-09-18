# Next steps

- [ ] Live-verify nvim `<leader>cl` forward search end-to-end in a real Ghostty
      session (socket send → page jump → 1s red flash → scroll near top).
      Headless + PTY coverage passed; the live GUI press is the acceptance test
      and cannot be run from the harness.
- [ ] Fix pre-existing `picker_labels_recent_files_with_parent_directory`
      failure: the directory title renders on the popup's border row
      (`Block::title`), but the test scans only the inner rows, so the
      expectation can never pass as written. Draw the label inside the rect or
      correct the expectation. Fails on clean HEAD `08996cb` (pre-dates forward
      search).

## Accepted caveats

- Forward-socket path steal: a second pdfterm instance unlinks and rebinds
  `/tmp/pdfterm-forward.sock` (last instance wins; the old viewer keeps an
  orphaned listener). Documented in `poll_forward_socket`; explicit error only
  if bind itself fails.
