//! Decoding the server's transport compression (D45,
//! [`skywave_proto::encoding`]): a body sent as one zstd frame is handed on
//! as its decoded bytes, so everything past the transport (hashes, sizes,
//! progress, `.part` offsets) counts decoded bytes.

use bytes::Bytes;
use hyper::header::{self, HeaderMap};
use skywave_proto::encoding::ZSTD;
use zstd::stream::raw::{Decoder, Operation};

use crate::transport::{BlobBody, BoxFuture, TransportError};

/// Decoded output taken per decoder call.
const SCRATCH: usize = 128 * 1024;

/// Whether `headers` say the body is a zstd frame.
pub(crate) fn is_zstd(headers: &HeaderMap) -> Result<bool, TransportError> {
    match headers.get(header::CONTENT_ENCODING) {
        None => Ok(false),
        Some(v) if v.as_bytes().eq_ignore_ascii_case(ZSTD.as_bytes()) => Ok(true),
        Some(other) => Err(TransportError::Decode(format!(
            "unknown Content-Encoding {other:?}"
        ))),
    }
}

/// A whole body as its decoded bytes: decompressed when `headers` say it is
/// a zstd frame, else as it came. The decompression runs off the core's
/// workers.
pub(crate) async fn decoded(headers: &HeaderMap, body: Bytes) -> Result<Bytes, TransportError> {
    if !is_zstd(headers)? {
        return Ok(body);
    }
    tokio::task::spawn_blocking(move || zstd::stream::decode_all(&body[..]).map(Bytes::from))
        .await
        .map_err(|e| e.to_string())
        .and_then(|decoded| decoded.map_err(|e| e.to_string()))
        .map_err(|e| TransportError::Decode(format!("decompressing a body: {e}")))
}

/// A body arriving as one zstd frame, handed on decoded chunk by chunk.
///
/// Every byte the decoder emits is a byte of the content, in order: a
/// compressed block comes out only once all of it arrived, a raw
/// (incompressible) block may come out as it streams in. So what a body
/// cut mid-frame handed on is a correct start of the decoded content, and
/// the download resumes from that decoded offset.
pub(crate) struct ZstdBody<B> {
    inner: B,
    decoder: Decoder<'static>,
    scratch: Vec<u8>,
    /// The frame's end was decoded; nothing may follow it.
    ended: bool,
}

impl<B: BlobBody> ZstdBody<B> {
    pub(crate) fn new(inner: B) -> Result<ZstdBody<B>, TransportError> {
        let decoder = Decoder::new()
            .map_err(|e| TransportError::Decode(format!("starting a decoder: {e}")))?;
        Ok(ZstdBody {
            inner,
            decoder,
            scratch: vec![0; SCRATCH],
            ended: false,
        })
    }

    /// All `input` decodes to, which may be nothing while a block is still
    /// arriving.
    fn decode(&mut self, input: &[u8]) -> Result<Vec<u8>, TransportError> {
        if self.ended {
            return Err(TransportError::Decode("bytes past the zstd frame".into()));
        }
        let mut decoded = Vec::new();
        let mut read = 0;
        loop {
            let status = self
                .decoder
                .run_on_buffers(&input[read..], &mut self.scratch)
                .map_err(|e| TransportError::Decode(format!("decompressing a body: {e}")))?;
            read += status.bytes_read;
            decoded.extend_from_slice(&self.scratch[..status.bytes_written]);
            if status.remaining == 0 {
                // The frame ended and is flushed.
                self.ended = true;
                if read < input.len() {
                    return Err(TransportError::Decode("bytes past the zstd frame".into()));
                }
                return Ok(decoded);
            }
            if read == input.len() && status.bytes_written < self.scratch.len() {
                return Ok(decoded);
            }
        }
    }
}

impl<B: BlobBody> BlobBody for ZstdBody<B> {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>> {
        Box::pin(async move {
            loop {
                let Some(input) = self.inner.next_chunk().await? else {
                    if self.ended {
                        return Ok(None);
                    }
                    // The body ended cleanly but the frame did not: the
                    // link lost its end, as a cut connection does.
                    return Err(TransportError::Io(
                        "body ended inside its zstd frame".into(),
                    ));
                };
                let decoded = self.decode(&input)?;
                if !decoded.is_empty() {
                    return Ok(Some(decoded.into()));
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A body that hands out `chunks`, then ends.
    struct Chunks(VecDeque<Bytes>);

    impl BlobBody for Chunks {
        fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>> {
            Box::pin(async move { Ok(self.0.pop_front()) })
        }
    }

    fn text(len: usize) -> Vec<u8> {
        (0..len).map(|i| b"abcdefgh\n"[i % 9]).collect()
    }

    async fn drain(body: &mut ZstdBody<Chunks>) -> (Vec<u8>, Option<TransportError>) {
        let mut got = Vec::new();
        loop {
            match body.next_chunk().await {
                Ok(Some(chunk)) => got.extend_from_slice(&chunk),
                Ok(None) => return (got, None),
                Err(e) => return (got, Some(e)),
            }
        }
    }

    fn split(frame: &[u8], at: usize) -> VecDeque<Bytes> {
        frame
            .chunks(at)
            .map(Bytes::copy_from_slice)
            .collect::<VecDeque<_>>()
    }

    /// A frame in TLS-record-sized pieces decodes to the content.
    #[tokio::test]
    async fn a_frame_in_pieces_decodes_whole() {
        let content = text(1_000_000);
        let frame = zstd::bulk::compress(&content, 3).unwrap();
        let mut body = ZstdBody::new(Chunks(split(&frame, 16 * 1024))).unwrap();
        let (got, error) = drain(&mut body).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(got, content);
    }

    /// A frame cut short hands on a correct start of the content, then
    /// fails as a broken link does, so the download resumes from there.
    #[tokio::test]
    async fn a_cut_frame_hands_on_a_correct_prefix_then_fails() {
        // Varied enough that its frame spans several blocks.
        let content: Vec<u8> = (0..2_000_000u32)
            .flat_map(|i| format!("{:x}\n", i.wrapping_mul(2_654_435_761)).into_bytes())
            .collect();
        let frame = zstd::bulk::compress(&content, 3).unwrap();
        let cut = frame.len() / 2;
        let mut body = ZstdBody::new(Chunks(split(&frame[..cut], 16 * 1024))).unwrap();
        let (got, error) = drain(&mut body).await;
        assert!(
            matches!(error, Some(TransportError::Io(_))),
            "a cut frame fails as the link: {error:?}"
        );
        assert!(
            !got.is_empty() && got.len() < content.len(),
            "{}",
            got.len()
        );
        assert_eq!(got, content[..got.len()]);
    }

    /// Bytes after the frame's end are refused rather than dropped.
    #[tokio::test]
    async fn bytes_past_the_frame_are_refused() {
        let mut frame = zstd::bulk::compress(&text(10_000), 3).unwrap();
        frame.extend_from_slice(b"junk");
        let mut body = ZstdBody::new(Chunks(split(&frame, 1 << 20))).unwrap();
        let (_, error) = drain(&mut body).await;
        assert!(
            matches!(error, Some(TransportError::Decode(_))),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_whole_body_decodes_only_when_marked() {
        let content = text(5000);
        let frame = Bytes::from(zstd::bulk::compress(&content, 3).unwrap());
        let mut headers = HeaderMap::new();
        assert_eq!(decoded(&headers, frame.clone()).await.unwrap(), frame);
        headers.insert(header::CONTENT_ENCODING, ZSTD.parse().unwrap());
        assert_eq!(decoded(&headers, frame.clone()).await.unwrap(), content);
        let cut = frame.slice(..frame.len() - 1);
        assert!(matches!(
            decoded(&headers, cut).await,
            Err(TransportError::Decode(_))
        ));
        headers.insert(header::CONTENT_ENCODING, "br".parse().unwrap());
        assert!(matches!(
            decoded(&headers, frame).await,
            Err(TransportError::Decode(_))
        ));
    }
}
