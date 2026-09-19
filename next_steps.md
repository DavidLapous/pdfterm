# Next steps

- [ ] Fix pre-existing `picker_labels_recent_files_with_parent_directory`:
      the directory label is absent even when scanning the whole popup rect.
      Failure also occurs on clean baseline `08996cb`.

- [ ] Isolate the forward-write EPIPE observed while Neovim had a file-change
      prompt open. Check scheduled write callbacks against the viewer's bounded
      receive deadline; causality has not been established.

## Accepted caveats

- Forward-socket path steal: a second pdfterm instance unlinks and rebinds
  `/tmp/pdfterm-forward.sock` (last instance wins; the old viewer keeps an
  orphaned listener). Documented in `poll_forward_socket`; explicit error only
  if bind itself fails.
  Separate instances can use different socket pairs through separate
  `XDG_CONFIG_HOME` configurations; document identity is still absent from the
  forward payload.
