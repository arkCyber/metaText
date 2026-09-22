/*!
 * framing.rs
 *
 * Length-prefixed message framing for the metaText core protocol.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Bounded frames: the declared length is validated before allocation
 * - Clean EOF detection, distinct from a truncated frame
 * - Generic over any `AsyncRead`/`AsyncWrite`, so one codec serves TCP and
 *   in-memory duplex pipes alike
 *
 * # Wire format
 *
 * ```text
 * +----------------+------------------------------+
 * | u32 big-endian | payload (UTF-8 JSON)         |
 * | length (>= 1)  | exactly `length` bytes       |
 * +----------------+------------------------------+
 * ```
 */

use std::io;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::protocol::MAX_FRAME_LEN;

/// Size of the big-endian length prefix.
pub const FRAME_HEADER_LEN: usize = 4;

/// Build an `InvalidData` I/O error carrying `message`.
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Read one frame.
///
/// # Returns
///
/// Returns `Ok(None)` at a clean end of stream, `Ok(Some(bytes))` for a
/// complete frame, and an error for a truncated or over-long frame.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] when the declared length is zero or
/// exceeds [`MAX_FRAME_LEN`], and the underlying I/O error for a read failure
/// or a truncated frame.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send,
{
    let mut header = [0_u8; FRAME_HEADER_LEN];

    // Read the first byte on its own so a clean EOF (peer closed the socket)
    // is distinguishable from a frame cut in the middle of its header.
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;

    let declared = u32::from_be_bytes(header);
    if declared == 0 {
        return Err(invalid("refusing to read an empty frame"));
    }
    if declared > MAX_FRAME_LEN {
        return Err(invalid(format!(
            "frame of {declared} bytes exceeds the {MAX_FRAME_LEN} byte limit"
        )));
    }

    // Only now is it safe to allocate: the length has been bounded.
    let mut payload = vec![0_u8; declared as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Write one frame and flush it.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] for an empty or over-long payload,
/// and the underlying I/O error when the write fails.
pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    if payload.is_empty() {
        return Err(invalid("refusing to write an empty frame"));
    }
    if payload.len() > MAX_FRAME_LEN as usize {
        return Err(invalid(format!(
            "frame of {} bytes exceeds the {MAX_FRAME_LEN} byte limit",
            payload.len()
        )));
    }

    // The length was checked against the frame limit above, so it fits in u32.
    #[allow(clippy::cast_possible_truncation)]
    let header = (payload.len() as u32).to_be_bytes();
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read and decode one frame.
///
/// # Errors
///
/// Propagates framing errors and reports malformed JSON as
/// [`io::ErrorKind::InvalidData`].
pub async fn read_message<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin + Send,
    T: DeserializeOwned + Send,
{
    let Some(bytes) = read_frame(reader).await? else {
        return Ok(None);
    };

    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| invalid(format!("malformed frame payload: {error}")))
}

/// Encode and write one message.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] when the message cannot be encoded,
/// and the underlying I/O error when the write fails.
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin + Send,
    T: Serialize + Sync,
{
    let bytes = serde_json::to_vec(message)
        .map_err(|error| invalid(format!("unserializable message: {error}")))?;
    write_frame(writer, &bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::{ClientMessage, Request};

    /// A message survives an in-memory duplex round trip.
    #[tokio::test]
    async fn test_frame_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let sent = ClientMessage::Request {
            id: 42,
            request: Request::Ping {
                echo: Some("hello".to_string()),
            },
        };

        let writer = tokio::spawn(async move {
            write_message(&mut client, &sent).await.expect("write");
        });

        let received: ClientMessage = read_message(&mut server)
            .await
            .expect("read")
            .expect("frame present");
        writer.await.expect("join");

        assert_eq!(
            received,
            ClientMessage::Request {
                id: 42,
                request: Request::Ping {
                    echo: Some("hello".to_string()),
                },
            }
        );
    }

    /// A closed stream yields `Ok(None)` rather than an error.
    #[tokio::test]
    async fn test_clean_eof_is_not_an_error() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        assert!(read_frame(&mut server).await.expect("clean eof").is_none());
    }

    /// An over-long declared length is rejected before allocation.
    #[tokio::test]
    async fn test_oversized_frame_is_rejected() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client
            .write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .await
            .expect("write header");

        let error = read_frame(&mut server).await.expect_err("must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// A zero length prefix is rejected.
    #[tokio::test]
    async fn test_empty_frame_is_rejected() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&0_u32.to_be_bytes()).await.expect("write");

        let error = read_frame(&mut server).await.expect_err("must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// A truncated frame is reported, not silently accepted.
    #[tokio::test]
    async fn test_truncated_frame_is_reported() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client
            .write_all(&10_u32.to_be_bytes())
            .await
            .expect("write");
        client.write_all(b"short").await.expect("write body");
        drop(client);

        let error = read_frame(&mut server).await.expect_err("must reject");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// The writer refuses empty payloads.
    #[tokio::test]
    async fn test_write_rejects_empty_payload() {
        let (mut client, _server) = tokio::io::duplex(64);
        let error = write_frame(&mut client, &[])
            .await
            .expect_err("must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// A deterministic pseudo random generator, so the robustness tests below
    /// are reproducible without pulling in a dependency.
    struct Corpus(u64);

    impl Corpus {
        /// Create a generator from a seed.
        const fn new(seed: u64) -> Self {
            Self(seed)
        }

        /// Next pseudo random 64-bit value (`SplitMix64`).
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Next pseudo random byte string of up to `max_len` bytes.
        fn bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = if max_len == 0 {
                0
            } else {
                // Compute the modulus in `u64` and convert the (small) result, so
                // the cast cannot truncate on a 32-bit target.
                usize::try_from(self.next() % (max_len as u64 + 1)).unwrap_or(0)
            };
            (0..len).map(|_| (self.next() & 0xff) as u8).collect()
        }
    }

    /// Arbitrary frame payloads survive a round trip byte for byte.
    ///
    /// This is the property the transport depends on: whatever is written by
    /// one peer must come out of the reader unchanged.
    #[tokio::test]
    async fn test_arbitrary_payloads_round_trip() {
        let mut corpus = Corpus::new(0x5EED);

        for _ in 0..200 {
            let payload = corpus.bytes(512);
            if payload.is_empty() {
                // Empty frames are rejected by design, covered elsewhere.
                continue;
            }

            let (mut writer, mut reader) = tokio::io::duplex(2048);
            let expected = payload.clone();
            let task = tokio::spawn(async move { write_frame(&mut writer, &expected).await });

            let received = read_frame(&mut reader)
                .await
                .expect("read must not fail")
                .expect("frame present");
            task.await.expect("join").expect("write");

            assert_eq!(received, payload);
        }
    }

    /// Arbitrary *unframed* input never panics and never over-allocates.
    #[tokio::test]
    async fn test_arbitrary_garbage_is_handled() {
        let mut corpus = Corpus::new(0x00C0_FFEE);

        for _ in 0..200 {
            let garbage = corpus.bytes(256);
            let (mut writer, mut reader) = tokio::io::duplex(4096);
            writer.write_all(&garbage).await.expect("write raw bytes");
            drop(writer);

            // Either a clean EOF, a decoded frame or a reported error; the
            // contract is "no panic and no unbounded allocation".
            if let Ok(Some(payload)) = read_frame(&mut reader).await {
                assert!(payload.len() <= MAX_FRAME_LEN as usize);
            }
        }
    }

    /// A reader that only ever delivers one byte at a time still reassembles
    /// frames: TCP does not preserve message boundaries.
    #[tokio::test]
    async fn test_frame_survives_one_byte_at_a_time_delivery() {
        let payload = vec![0xAB; 64];
        let (mut writer, mut reader) = tokio::io::duplex(1);

        let expected = payload.clone();
        let task = tokio::spawn(async move {
            write_frame(&mut writer, &expected).await.expect("write");
        });

        let received = read_frame(&mut reader)
            .await
            .expect("read must not fail")
            .expect("frame present");
        task.await.expect("join");

        assert_eq!(received, payload);
    }

    /// A frame at exactly the size limit is accepted; one byte more is not.
    #[tokio::test]
    async fn test_frame_size_boundary() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(&MAX_FRAME_LEN.to_be_bytes())
            .await
            .expect("write header");
        // Announce exactly the limit, then close: the reader must try to read
        // the body rather than rejecting the header.
        drop(writer);
        let error = read_frame(&mut reader).await.expect_err("body is missing");
        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "the header itself must be accepted"
        );
    }

    /// Malformed payloads are reported as data errors, not as panics.
    #[tokio::test]
    async fn test_malformed_payload_is_a_data_error() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let task = tokio::spawn(async move {
            write_frame(&mut writer, b"{not json").await.expect("write");
        });

        let error = read_message::<_, crate::ipc::protocol::ClientMessage>(&mut reader)
            .await
            .expect_err("must be rejected");
        task.await.expect("join");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
