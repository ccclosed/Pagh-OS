// shell/cat.rs — the pure streaming core of the `cat` command
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// `commands::cmd_cat` is thin VFS + console glue; everything it decides lives
// here, so the in-kernel property suite (`src/test.rs`, `shell_prop_tests`)
// drives the exact byte sequence a user gets without a VFS or a console:
//
// * read in 256-byte chunks, echoing every byte verbatim and in order;
// * stop at EOF, at the [`MAX_BYTES`] cap, or on the first read error;
// * finish the transcript on a fresh line: when something was echoed and the
//   last byte is not `\n`, echo one. Without it the next shell prompt is glued
//   onto the file's last line (`hello-pagh:/>`), which corrupts any transcript
//   a human or an E2E script reads (QA finding F-04).

/// Byte cap: `cat` refuses to spin forever on an endless device.
pub(crate) const MAX_BYTES: u64 = 64 * 1024;

/// Read request size. Small on purpose: the whole point of streaming is that
/// `cat` never buffers the file, so this is stack, not heap.
const CHUNK: usize = 256;

/// What one [`stream`] call did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Streamed {
    /// File bytes echoed (the offset the stream stopped at).
    pub bytes: u64,
    /// Whether a terminating `\n` was appended because the last echoed byte
    /// was not one.
    pub newline_added: bool,
    /// A read returned an error after `bytes` bytes had been echoed.
    pub read_failed: bool,
}

/// Echo a file through `write` until EOF / [`MAX_BYTES`] / a read error, then
/// terminate the transcript with `\n` unless the last echoed byte already is
/// one.
///
/// `read(offset, buf)` places at most `buf.len()` bytes at `buf`, returns the
/// count (`Ok(0)` is EOF) and `Err(())` on a VFS error; `write` receives the
/// bytes in order. The contract is exact so a test can assert it:
///
/// * the echoed bytes are the file's first [`Streamed::bytes`] bytes, capped at
///   [`MAX_BYTES`];
/// * an empty file produces no output at all (and no newline);
/// * when at least one byte was echoed and the last one is not `b'\n'`, exactly
///   one `b"\n"` follows — including after a read error, so the caller's error
///   message starts on its own line;
/// * the caller learns about the error through [`Streamed::read_failed`] only
///   after that terminator was emitted.
pub(crate) fn stream<R, W>(mut read: R, mut write: W) -> Streamed
where
    R: FnMut(u64, &mut [u8]) -> Result<usize, ()>,
    W: FnMut(&[u8]),
{
    let mut buf = [0u8; CHUNK];
    let mut bytes: u64 = 0;
    let mut last: Option<u8> = None;
    let mut read_failed = false;
    while bytes < MAX_BYTES {
        // Never ask for more than the cap allows, so the cap is exact.
        let want = ((MAX_BYTES - bytes).min(CHUNK as u64)) as usize;
        match read(bytes, &mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                // Defensive: a broken `read` must not index past the request.
                let n = n.min(want);
                if n == 0 {
                    break;
                }
                write(&buf[..n]);
                last = Some(buf[n - 1]);
                bytes += n as u64;
            }
            Err(()) => {
                read_failed = true;
                break;
            }
        }
    }
    let newline_added = matches!(last, Some(b) if b != b'\n');
    if newline_added {
        write(b"\n");
    }
    Streamed {
        bytes,
        newline_added,
        read_failed,
    }
}
