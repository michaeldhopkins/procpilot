//! Stdin input for [`Cmd`](crate::Cmd).
//!
//! Modeled after the subprocess crate's `InputData`. Accepts owned bytes
//! (which can be re-fed across retries) or a boxed `Read` (one-shot —
//! retries with a Reader-stdin will fail on the second attempt).

use std::io::Read;

/// Input data to feed to a child process's stdin.
///
/// Construct via the `From` impls — accepts `Vec<u8>`, `String`, `&str`,
/// `&[u8]`, or `Box<dyn Read + Send + Sync + 'static>`. Use
/// [`from_reader()`](Self::from_reader) for the streaming case.
///
/// # Retry semantics
///
/// - [`Bytes`](Self::Bytes): owned, reusable. If the [`Cmd`](crate::Cmd) is
///   configured to retry, each attempt re-feeds the same bytes.
/// - [`Reader`](Self::Reader): one-shot. The first attempt consumes the
///   reader. If a retry is needed, no stdin is available and the second
///   attempt will likely behave differently. Avoid `.retry()` with a
///   streaming reader unless you understand this.
pub enum StdinData {
    Bytes(Vec<u8>),
    Reader(Box<dyn Read + Send + Sync + 'static>),
}

impl StdinData {
    /// Wrap any `Read` source as one-shot streaming stdin.
    pub fn from_reader<R: Read + Send + Sync + 'static>(reader: R) -> Self {
        Self::Reader(Box::new(reader))
    }

    /// Whether this stdin can be safely re-used across retries.
    pub fn is_reusable(&self) -> bool {
        matches!(self, Self::Bytes(_))
    }
}

impl std::fmt::Debug for StdinData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(b) => f
                .debug_struct("Bytes")
                .field("len", &b.len())
                .finish(),
            Self::Reader(_) => f.debug_struct("Reader").finish_non_exhaustive(),
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
        assert!(data.is_reusable());
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
    fn from_reader_marks_one_shot() {
        let data = StdinData::from_reader(Cursor::new(vec![1, 2, 3]));
        assert!(!data.is_reusable());
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
}
