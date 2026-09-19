# Next steps

- [ ] Fix pre-existing `picker_labels_recent_files_with_parent_directory`:
      the directory label is absent even when scanning the whole popup rect.
      Failure also occurs on clean baseline `08996cb`.

- [ ] Exercise build, rendering, generic editor commands, private sockets, and
      Kitty lifecycle on Linux when a Linux machine is available. Ghostty's
      current editor adapter uses macOS AppleScript.

## Accepted limits

- One viewer and one editor socket adapter per configuration; a viewer may hold
  multiple PDF tabs. Separate configurations isolate independent sessions.
- A crash or SIGKILL can leave an endpoint requiring explicit stale-file removal.
- PDFium runs in-process without a sandbox; do not treat untrusted PDFs as safe.
- Source-word precision requires saved source and an unambiguous prose match.
