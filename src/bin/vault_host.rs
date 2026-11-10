//! Chromium native-messaging host: relays 4-byte native-endian framed JSON on
//! stdin/stdout to the JSON-lines socket of `nexus-vaultd`. Bytes only: the
//! host never parses or keeps a request, so it never holds a password longer
//! than one relay.

use std::env;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// Chromium caps what an extension may send a host at 4 GiB but what a host
/// may send back at 1 MiB; a vault request has no reason to approach either.
const MAX_IN: usize = 1024 * 1024;
const MAX_OUT: usize = 1024 * 1024;

/// One framed message, `None` on clean EOF. An EOF inside a frame is an error.
fn read_frame(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match input.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_ne_bytes(header) as usize;
    if len == 0 || len > MAX_IN {
        return Err(io::Error::new(ErrorKind::InvalidData, "bad frame length"));
    }
    let mut body = vec![0; len];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(output: &mut impl Write, body: &[u8]) -> io::Result<()> {
    output.write_all(&(body.len() as u32).to_ne_bytes())?;
    output.write_all(body)?;
    output.flush()
}

/// JSON escapes newlines inside strings, so a raw newline can only mean a
/// malformed message, which would otherwise split into two socket requests.
fn relay(
    body: &[u8],
    daemon: &mut (impl Read + Write),
    reply: &mut impl BufRead,
) -> io::Result<Vec<u8>> {
    if body.contains(&b'\n') {
        return Err(io::Error::new(ErrorKind::InvalidData, "newline in request"));
    }
    daemon.write_all(body)?;
    daemon.write_all(b"\n")?;
    daemon.flush()?;
    let mut line = Vec::new();
    reply
        .take(MAX_OUT as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_OUT {
            io::Error::new(ErrorKind::InvalidData, "reply too large")
        } else {
            ErrorKind::UnexpectedEof.into()
        });
    }
    line.pop();
    Ok(line)
}

fn socket_path() -> Option<PathBuf> {
    Some(PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?).join("nexus/vault.sock"))
}

/// Connect, starting the `nexus-vaultd` beside this binary if nothing listens.
fn connect() -> io::Result<UnixStream> {
    let path = socket_path().ok_or(ErrorKind::NotFound)?;
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    let daemon = env::current_exe()?.with_file_name("nexus-vaultd");
    Command::new(daemon)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut last = ErrorKind::NotFound.into();
    for _ in 0..50 {
        sleep(Duration::from_millis(100));
        match UnixStream::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// One connection per message: Chromium starts this host once per
/// `sendNativeMessage`, and vaultd exits when idle, so nothing long-lived to keep.
fn exchange(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = connect()?;
    let mut reply = BufReader::new(stream.try_clone()?);
    relay(body, &mut stream, &mut reply)
}

fn failure(error: &str) -> Vec<u8> {
    format!("{{\"ok\":false,\"error\":\"{error}\"}}").into_bytes()
}

fn serve(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    while let Some(body) = read_frame(input)? {
        let reply = exchange(&body).unwrap_or_else(|e| match e.kind() {
            ErrorKind::InvalidData => failure("invalid message"),
            _ => failure("vaultd unavailable"),
        });
        write_frame(output, &reply)?;
    }
    Ok(())
}

fn main() {
    let result = serve(&mut io::stdin().lock(), &mut io::stdout().lock());
    if let Err(e) = result {
        eprintln!("nexus-vault-host: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Yields one byte per read, the worst partial-read pattern a pipe can give.
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.0 = rest;
            Ok(1)
        }
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        let mut out = (body.len() as u32).to_ne_bytes().to_vec();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn partial_reads_reassemble() {
        let mut bytes = framed(b"{\"op\":\"status\"}");
        bytes.extend(framed(b"{\"op\":\"lock\"}"));
        let mut input = Trickle(&bytes);
        assert_eq!(
            read_frame(&mut input).unwrap().unwrap(),
            b"{\"op\":\"status\"}"
        );
        assert_eq!(
            read_frame(&mut input).unwrap().unwrap(),
            b"{\"op\":\"lock\"}"
        );
        assert!(read_frame(&mut input).unwrap().is_none());
    }

    #[test]
    fn rejects_bad_lengths_before_allocating() {
        for len in [0u32, MAX_IN as u32 + 1, u32::MAX] {
            let mut input = Cursor::new(len.to_ne_bytes().to_vec());
            let err = read_frame(&mut input).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidData, "{len}");
        }
        let exact = vec![b'x'; MAX_IN];
        assert_eq!(
            read_frame(&mut Cursor::new(framed(&exact)))
                .unwrap()
                .unwrap()
                .len(),
            MAX_IN
        );
    }

    #[test]
    fn truncation_is_an_error_not_eof() {
        assert!(read_frame(&mut Cursor::new(vec![1, 0])).is_err());
        let mut short = framed(b"abcdef");
        short.truncate(7);
        assert!(read_frame(&mut Cursor::new(short)).is_err());
    }

    struct Daemon {
        sent: Vec<u8>,
    }
    impl Read for Daemon {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }
    impl Write for Daemon {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn relay_is_one_line_each_way() {
        let mut daemon = Daemon { sent: vec![] };
        let mut reply = Cursor::new(b"{\"ok\":true}\nleftover".to_vec());
        assert_eq!(
            relay(b"{}", &mut daemon, &mut reply).unwrap(),
            b"{\"ok\":true}"
        );
        assert_eq!(daemon.sent, b"{}\n");
    }

    #[test]
    fn newline_cannot_smuggle_a_second_request() {
        let mut daemon = Daemon { sent: vec![] };
        let err = relay(
            b"{}\n{\"op\":\"lock\"}",
            &mut daemon,
            &mut Cursor::new(vec![]),
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(daemon.sent.is_empty());
    }

    #[test]
    fn oversized_or_cut_replies_fail() {
        let mut daemon = Daemon { sent: vec![] };
        let mut huge = vec![b'x'; MAX_OUT + 2];
        huge.push(b'\n');
        let err = relay(b"{}", &mut daemon, &mut Cursor::new(huge)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        let err = relay(b"{}", &mut daemon, &mut Cursor::new(b"{\"ok\"".to_vec())).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }
}
