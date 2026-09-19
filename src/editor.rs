use crate::synctex::{ForwardRequest, SourceLocation};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant};

/// Editor selection is trusted configuration. Document paths are arguments, never shell code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum Editor {
    #[default]
    None,
    Socket {
        path: String,
    },
    Command {
        argv: Vec<String>,
    },
}

impl Editor {
    pub(crate) fn socket_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Socket { path } => Some(path),
            _ => None,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Socket { path } if path.is_empty() => {
                Err(io::Error::other("editor socket path must not be empty"))
            }
            Self::Command { argv } => {
                if argv.is_empty() || argv[0].is_empty() || argv[0].contains(['{', '}']) {
                    return Err(io::Error::other(
                        "editor command requires a literal executable as argv[0]",
                    ));
                }
                let sample = SourceLocation {
                    file: String::new(),
                    line: 1,
                    byte_column: 0,
                    column: 1,
                    column_char: 1,
                    precise: false,
                };
                for argument in &argv[1..] {
                    expand(argument, &sample)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn deliver(&self, location: &SourceLocation) -> io::Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Socket { path } => {
                let mut stream = UnixStream::connect(path)?;
                stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                serde_json::to_writer(&mut stream, location)?;
                stream.shutdown(Shutdown::Write)
            }
            Self::Command { argv } => {
                self.validate()?;
                let arguments = argv[1..]
                    .iter()
                    .map(|argument| expand(argument, location))
                    .collect::<io::Result<Vec<_>>>()?;
                let output = Command::new(&argv[0]).args(arguments).output()?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "editor command exited with {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    )))
                }
            }
        }
    }
}

fn expand(template: &str, location: &SourceLocation) -> io::Result<String> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .map(|offset| start + offset)
            .ok_or_else(|| io::Error::other("unclosed editor placeholder"))?;
        let value = match &rest[start + 1..end] {
            "file" => location.file.clone(),
            "line" => location.line.to_string(),
            "column" => location.column.to_string(),
            "column_char" => location.column_char.to_string(),
            "byte_column" => location.byte_column.to_string(),
            "column_byte" => (location.byte_column + 1).to_string(),
            unknown => {
                return Err(io::Error::other(format!(
                    "unknown editor placeholder: {unknown}"
                )));
            }
        };
        result.push_str(&value);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub ok: bool,
    pub error: Option<String>,
}

pub(crate) const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

/// One terminal reply per connection, including unwinding/normal viewer shutdown.
pub(crate) struct ForwardReply(Option<UnixStream>);

impl ForwardReply {
    pub fn new(stream: UnixStream) -> Self {
        Self(Some(stream))
    }

    pub fn disconnected(&self) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let Some(stream) = self.0.as_ref() else {
            return Ok(true);
        };
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // Poll the WRITE side: on macOS, a read-side HUP also occurs for the
        // normal request half-close, and events=0 does not report peer closure.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0)
    }

    pub fn finish(&mut self, error: Option<String>) {
        let Some(mut stream) = self.0.take() else {
            return;
        };
        let error = error.map(|message| {
            if message.len() <= 512 {
                message
            } else {
                format!("{}…", message.chars().take(512).collect::<String>())
            }
        });
        let reply = Reply {
            ok: error.is_none(),
            error,
        };
        let result = (|| -> io::Result<()> {
            let bytes = serde_json::to_vec(&reply)?;
            let deadline = Instant::now() + Duration::from_millis(100);
            let mut remaining = bytes.as_slice();
            while !remaining.is_empty() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "forward reply write timed out",
                    ));
                }
                match stream.write(remaining) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(count) => remaining = &remaining[count..],
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            stream.shutdown(Shutdown::Write)
        })();
        if let Err(error) = result {
            eprintln!("pdfterm: forward reply failed: {error}");
        }
    }
}

impl Drop for ForwardReply {
    fn drop(&mut self) {
        self.finish(Some(
            "viewer stopped before forward frame submission".into(),
        ));
    }
}

pub fn forward(path: &str, request: &ForwardRequest) -> io::Result<()> {
    request.validate()?;
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.set_read_timeout(Some(FORWARD_TIMEOUT + Duration::from_secs(1)))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    stream.take(4097).read_to_string(&mut response)?;
    if response.len() > 4096 {
        return Err(io::Error::other("forward reply exceeds 4096 bytes"));
    }
    let reply: Reply = serde_json::from_str(&response)?;
    if reply.ok {
        Ok(())
    } else {
        Err(io::Error::other(
            reply
                .error
                .unwrap_or_else(|| "forward request rejected".into()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_reply_waits_for_submission_and_survives_request_half_close() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();
        let mut reply = ForwardReply::new(server);
        assert!(!reply.disconnected().unwrap());
        let mut byte = [0];
        assert_eq!(
            client.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        reply.finish(None);
        drop(reply);
        client.set_nonblocking(false).unwrap();
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(response.ok);
        assert!(response.error.is_none());
    }

    #[test]
    fn abandoned_forward_reply_reports_failure_and_detects_disconnection() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        drop(ForwardReply::new(server));
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(!response.ok);
        assert!(response.error.is_some());

        let (client, server) = UnixStream::pair().unwrap();
        let reply = ForwardReply::new(server);
        drop(client);
        assert!(reply.disconnected().unwrap());
    }

    #[test]
    fn escaped_forward_errors_fit_the_reply_limit() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        ForwardReply::new(server).finish(Some("\0".repeat(2048)));
        let mut payload = String::new();
        client.read_to_string(&mut payload).unwrap();
        assert!(payload.len() <= 4096);
        let response: Reply = serde_json::from_str(&payload).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().starts_with('\0'));
    }

    #[test]
    fn command_delivers_literal_filename_and_explicit_columns() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("args");
        let editor = Editor::Command {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\n' \"$@\" > \"$0\"".into(),
                output.to_str().unwrap().into(),
                "{file}".into(),
                "{line}:{column}:{column_char}:{byte_column}:{column_byte}".into(),
            ],
        };
        let location = SourceLocation {
            file: "$(touch should-not-exist); {line} ' \".tex".into(),
            line: 7,
            byte_column: 5,
            column: 4,
            column_char: 3,
            precise: true,
        };
        editor.deliver(&location).unwrap();
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            format!("{}\n7:4:3:5:6\n", location.file)
        );
        assert!(
            Editor::Command {
                argv: vec!["false".into()]
            }
            .deliver(&location)
            .is_err()
        );
        assert!(
            Editor::Command {
                argv: vec!["echo".into(), "{typo}".into()]
            }
            .validate()
            .is_err()
        );
    }
}
