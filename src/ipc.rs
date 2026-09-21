use crate::synctex::{ForwardRequest, parse_forward_request};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Cancellable connection, including a saturated Unix listener backlog.
pub(crate) fn connect(path: &str, operation: &crate::process::Operation) -> io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    operation.check()?;
    // Zero initialization supplies the trailing NUL and optional platform fields.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_bytes();
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::other("invalid Unix socket path"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as _;
    }
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of_val(&address) as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        operation.check()?;
        let result = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as _,
            )
        };
        if result == 0 {
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => return Ok(stream),
            Some(libc::EINPROGRESS | libc::EALREADY) => loop {
                operation.check()?;
                let mut descriptor = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, 2) };
                if ready < 0 {
                    return Err(io::Error::last_os_error());
                }
                if ready > 0 {
                    if let Some(error) = stream.take_error()? {
                        return Err(error);
                    }
                    return Ok(stream);
                }
            },
            Some(libc::EAGAIN | libc::EINTR) => std::thread::sleep(Duration::from_millis(2)),
            _ => return Err(error),
        }
    }
}

pub(crate) fn send(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    operation: &crate::process::Operation,
) -> io::Result<()> {
    while !bytes.is_empty() {
        operation.check()?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    stream.shutdown(Shutdown::Write)
}
/// Only the current user may traverse directories containing IPC or native code.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid takes no arguments and has no failure mode.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a real directory owned by the current user with mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) struct Listener {
    pub socket: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
}

impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        private_dir(
            path.parent()
                .ok_or_else(|| io::Error::other("socket path has no parent"))?,
        )?;
        // Never unlink a live listener, symlink, or unrelated file. A crash leaves
        // a stale socket that its owner must explicitly remove before restarting.
        let socket = UnixListener::bind(path).map_err(|error| {
            io::Error::new(error.kind(), format!("cannot bind {}: {error}; an existing socket must be stopped or explicitly removed", path.display()))
        })?;
        let metadata = fs::symlink_metadata(path)?;
        let listener = Self {
            socket,
            path: path.to_owned(),
            identity: (metadata.dev(), metadata.ino()),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.socket.set_nonblocking(true)?;
        Ok(listener)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
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
            stream.set_nonblocking(true)?;
            // A reply fits below 4 KiB. Never wait for an unresponsive client.
            stream.write_all(&bytes)?;
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

/// Incremental request reads. A slow client never sleeps on the event thread.
struct Incoming {
    stream: UnixStream,
    bytes: Vec<u8>,
    deadline: Instant,
}
impl Incoming {
    fn poll(&mut self) -> Option<io::Result<String>> {
        let mut buffer = [0; 4097];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    return Some(
                        String::from_utf8(std::mem::take(&mut self.bytes))
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
                    );
                }
                Ok(n) => {
                    if self.bytes.len() + n > 4096 {
                        return Some(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "forward request exceeds 4096 bytes",
                        )));
                    }
                    self.bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(e)),
            }
        }
        (Instant::now() >= self.deadline).then(|| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "forward request did not reach EOF within 100ms",
            ))
        })
    }
}

pub(crate) struct ForwardListener {
    listener: Listener,
    clients: Vec<Incoming>,
}
impl ForwardListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        Ok(Self {
            listener: Listener::bind(path)?,
            clients: Vec::new(),
        })
    }
    pub fn poll(&mut self) -> io::Result<Vec<(io::Result<ForwardRequest>, ForwardReply)>> {
        // Bound acceptance and work even when local clients flood the socket.
        for _ in self.clients.len()..16 {
            match self.listener.socket.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    self.clients.push(Incoming {
                        stream,
                        bytes: Vec::new(),
                        deadline: Instant::now() + Duration::from_millis(100),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.clients.len() {
            if let Some(result) = self.clients[index].poll() {
                let client = self.clients.remove(index);
                ready.push((
                    result.and_then(|payload| parse_forward_request(&payload)),
                    ForwardReply::new(client.stream),
                ));
            } else {
                index += 1;
            }
        }
        Ok(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
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
    fn refuses_public_or_symlink_directories() {
        let root = tempfile::tempdir().unwrap();
        let public = root.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            private_dir(&public).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let link = root.path().join("link");
        symlink(root.path(), &link).unwrap();
        assert_eq!(
            private_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn listener_never_steals_paths_and_cleans_only_itself() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private/viewer.sock");
        let first = Listener::bind(&path).unwrap();
        assert!(Listener::bind(&path).is_err());
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        drop(first);
        assert!(!path.exists());
        fs::write(&path, "unrelated").unwrap();
        assert!(Listener::bind(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "unrelated");
        fs::remove_file(&path).unwrap();
        let listener = Listener::bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();
        drop(listener);
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement");
    }

    #[test]
    fn incomplete_clients_do_not_block_complete_requests_or_future_connections() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private/forward.sock");
        let mut listener = ForwardListener::bind(&path).unwrap();
        let mut slow = UnixStream::connect(&path).unwrap();
        slow.write_all(b"{").unwrap();
        assert!(listener.poll().unwrap().is_empty());
        let mut fast = UnixStream::connect(&path).unwrap();
        fast.write_all(b"{}").unwrap();
        fast.shutdown(Shutdown::Write).unwrap();
        let ready = listener.poll().unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(listener.clients.len(), 1);
        assert!(ready[0].0.is_err()); // Complete but invalid JSON request.
        listener.clients[0].deadline = Instant::now();
        let expired = listener.poll().unwrap();
        assert_eq!(
            expired[0].0.as_ref().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let mut next = UnixStream::connect(&path).unwrap();
        next.write_all(&vec![b'x'; 4097]).unwrap();
        drop(next); // Also exercises a full peer close before acceptance on macOS.
        let oversized = listener.poll().unwrap();
        assert_eq!(oversized.len(), 1);
        assert!(
            oversized[0]
                .0
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("4096")
        );
    }

    #[test]
    fn editor_socket_backpressure_observes_navigation_deadline() {
        let (mut sender, _receiver) = UnixStream::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        let operation = crate::process::Operation::new(Duration::from_millis(30));
        let error = send(&mut sender, &vec![0; 4 * 1024 * 1024], &operation).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
