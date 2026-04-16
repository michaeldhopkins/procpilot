//! Stdin input for [`Cmd`](crate::Cmd).
//!
//! Three variants cover the common cases:
//!
//! - [`Bytes`](StdinData::Bytes) — owned, reusable across retries.
//! - [`Reader`](StdinData::Reader) — one-shot sync `Read`.
//! - [`AsyncReader`](StdinData::AsyncReader) — one-shot async `AsyncRead`,
//!   gated on the `tokio` feature.
//!
//! Pick by what you have and where you'll run: bytes when the input is
//! small or retry needs to re-feed; sync `Reader` when you have a
//! `std::io::Read` on the sync runner; `AsyncReader` for true async
//! streaming of large inputs on the tokio runner.

use std::io::Read;

/// Input data to feed to a child process's stdin.
///
/// Construct via the `From` impls for bytes/strings, or
/// [`from_reader`](Self::from_reader) / [`from_async_reader`](Self::from_async_reader)
/// for streaming sources.
///
/// # Retry and clone semantics
///
/// - [`Bytes`](Self::Bytes): owned. If the [`Cmd`](crate::Cmd) is
///   configured to retry, each attempt re-feeds the same bytes (internally
///   the buffer is `Arc`-shared for cheap clones).
/// - [`Reader`](Self::Reader): one-shot. The first run attempt consumes
///   the reader; subsequent retries or cloned-then-run attempts see no
///   stdin. Avoid `.retry()` with a reader unless you understand this.
/// - [`AsyncReader`](Self::AsyncReader): one-shot like `Reader`. Only
///   usable on the async path ([`Cmd::run_async`](crate::Cmd::run_async) /
///   [`Cmd::spawn_async`](crate::Cmd::spawn_async)). Passing it to the
///   sync runner surfaces a [`RunError::Spawn`](crate::RunError::Spawn)
///   with `ErrorKind::InvalidInput`.
///
/// # Picking the right variant for large inputs
///
/// For inputs larger than a few MB, the sync `Reader` variant streams
/// through an OS pipe without buffering the whole thing in memory. On the
/// async path, `Reader` would require `spawn_blocking` to drain the sync
/// reader fully into memory before writing — not suitable for
/// gigabyte-sized inputs. Use [`from_async_reader`](Self::from_async_reader)
/// with a `tokio::fs::File` or any other `AsyncRead` source for async
/// streaming.
#[non_exhaustive]
pub enum StdinData {
    /// Owned bytes. Reusable across retries.
    Bytes(Vec<u8>),
    /// One-shot sync reader. The reader only needs `Send`; it doesn't have
    /// to be `Sync` because procpilot hands it to at most one thread at a
    /// time (via `Arc<Mutex<Option<_>>>`).
    Reader(Box<dyn Read + Send + 'static>),
    /// One-shot async reader; only usable with `run_async` / `spawn_async`.
    #[cfg(feature = "tokio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    AsyncReader(Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>),
}

impl StdinData {
    /// Wrap any sync `Read` source as one-shot streaming stdin.
    pub fn from_reader<R: Read + Send + 'static>(reader: R) -> Self {
        Self::Reader(Box::new(reader))
    }

    /// Wrap any `tokio::io::AsyncRead` source as one-shot streaming stdin
    /// for the async path. Pairs with
    /// [`Cmd::run_async`](crate::Cmd::run_async) and
    /// [`Cmd::spawn_async`](crate::Cmd::spawn_async).
    ///
    /// Passing the resulting `StdinData` to the sync
    /// [`Cmd::run`](crate::Cmd::run) or [`Cmd::spawn`](crate::Cmd::spawn)
    /// surfaces a [`RunError::Spawn`](crate::RunError::Spawn) with
    /// `ErrorKind::InvalidInput` — the sync runner has no way to drive an
    /// async source.
    #[cfg(feature = "tokio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    pub fn from_async_reader<R: tokio::io::AsyncRead + Send + Unpin + 'static>(
        reader: R,
    ) -> Self {
        Self::AsyncReader(Box::new(reader))
    }

}

impl std::fmt::Debug for StdinData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(b) => f.debug_struct("Bytes").field("len", &b.len()).finish(),
            Self::Reader(_) => f.debug_struct("Reader").finish_non_exhaustive(),
            #[cfg(feature = "tokio")]
            Self::AsyncReader(_) => f.debug_struct("AsyncReader").finish_non_exhaustive(),
        }
    }
}

impl From<Vec<u8>> for StdinData {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}

impl From<&[u8]> for StdinData {
    fn from(s: &[u8]) -> Self {
        Self::Bytes(s.to_vec())
    }
}

impl From<&Vec<u8>> for StdinData {
    fn from(v: &Vec<u8>) -> Self {
        Self::Bytes(v.clone())
    }
}

impl From<String> for StdinData {
    fn from(s: String) -> Self {
        Self::Bytes(s.into_bytes())
    }
}

impl From<&str> for StdinData {
    fn from(s: &str) -> Self {
        Self::Bytes(s.as_bytes().to_vec())
    }
}

impl From<&String> for StdinData {
    fn from(s: &String) -> Self {
        Self::Bytes(s.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn from_vec_u8() {
        let data: StdinData = vec![1, 2, 3].into();
        match data {
            StdinData::Bytes(b) => assert_eq!(b, vec![1, 2, 3]),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn from_byte_slice() {
        let data: StdinData = (&b"hello"[..]).into();
        match data {
            StdinData::Bytes(b) => assert_eq!(b, b"hello"),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn from_string() {
        let data: StdinData = String::from("hello").into();
        match data {
            StdinData::Bytes(b) => assert_eq!(b, b"hello"),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn from_str() {
        let data: StdinData = "hello".into();
        match data {
            StdinData::Bytes(b) => assert_eq!(b, b"hello"),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn from_reader_produces_reader_variant() {
        let data = StdinData::from_reader(Cursor::new(vec![1, 2, 3]));
        assert!(matches!(data, StdinData::Reader(_)));
    }

    #[test]
    fn debug_impl_redacts_reader_contents() {
        let bytes_dbg = format!("{:?}", StdinData::Bytes(vec![0; 100]));
        assert!(bytes_dbg.contains("100"));

        let reader_dbg = format!(
            "{:?}",
            StdinData::from_reader(Cursor::new(vec![0u8; 100]))
        );
        assert!(reader_dbg.contains("Reader"));
        assert!(!reader_dbg.contains("100"));
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn from_async_reader_produces_async_reader_variant() {
        use tokio::io::AsyncRead;
        struct Empty;
        impl AsyncRead for Empty {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let data = StdinData::from_async_reader(Empty);
        assert!(matches!(data, StdinData::AsyncReader(_)));
    }
}
