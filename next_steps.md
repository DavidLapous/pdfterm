# Next steps

- [ ] Fix pre-existing `picker_labels_recent_files_with_parent_directory`:
      the directory label is absent even when scanning the whole popup rect.
      Failure also occurs on clean baseline `08996cb`.

## Accepted caveats

- Forward-socket path steal: a second pdfterm instance unlinks and rebinds
  `/tmp/pdfterm-forward.sock` (last instance wins; the old viewer keeps an
  orphaned listener). Documented in `poll_forward_socket`; explicit error only
  if bind itself fails.
