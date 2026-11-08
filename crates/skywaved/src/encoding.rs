//! Transport compression of event batches and blob bodies (D45,
//! [`skywave_proto::encoding`]): a body of at least
//! [`COMPRESS_MIN_BYTES`] for a request that accepts zstd goes as one zstd
//! frame at [`LEVEL`], encoded on blocking threads.

use std::fs::File;
use std::io::{self, Read, Take, Write};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use skywave_proto::encoding::{COMPRESS_MIN_BYTES, ZSTD, accepts_zstd};

/// zstd's own default: some 3× on text, and fast enough even on a small
/// machine that the link, not the CPU, bounds every transfer.
pub const LEVEL: i32 = 3;

/// Read size when streaming a blob.
pub(crate) const BLOB_CHUNK: usize = 64 * 1024;

/// Whether the request's `Accept-Encoding` asks for zstd.
pub(crate) fn wants_zstd(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(accepts_zstd)
}

/// Whether a body of `len` bytes is compressed for a request that asked.
pub(crate) fn worth_compressing(len: u64) -> bool {
    len >= COMPRESS_MIN_BYTES
}

/// `value` as a 200 JSON response, one zstd frame when `compress` and it is
/// worth it. Serializing and compressing run on a blocking thread: a full
/// batch is megabytes.
pub(crate) async fn json<T: serde::Serialize + Send + 'static>(
    value: T,
    compress: bool,
) -> Response {
    let encoded = tokio::task::spawn_blocking(move || -> io::Result<(Vec<u8>, bool)> {
        let json = serde_json::to_vec(&value)?;
        if compress && worth_compressing(json.len() as u64) {
            Ok((zstd::bulk::compress(&json, LEVEL)?, true))
        } else {
            Ok((json, false))
        }
    })
    .await;
    let (body, compressed) = match encoded {
        Ok(Ok(encoded)) => encoded,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "encoding a response");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "encoding a response panicked");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut resp = Response::new(Body::from(body));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if compressed {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(ZSTD));
    }
    resp
}

/// The body of a compressed blob response: one zstd frame of `len` bytes
/// of `file` from where it stands, made a piece at a time on blocking
/// threads. A piece is what the encoder emits after one or more
/// [`BLOB_CHUNK`] reads (it buffers up to a block, 128 KiB of input); the
/// last one ends the frame.
pub(crate) fn blob_frame(file: File, len: u64) -> Body {
    let frames = Frames {
        file: file.take(len),
        encoder: None,
        done: false,
    };
    let pieces = futures_util::stream::try_unfold(frames, |mut frames| async move {
        let (frames, piece) = tokio::task::spawn_blocking(move || {
            let piece = frames.next_piece();
            (frames, piece)
        })
        .await
        .map_err(io::Error::other)?;
        Ok::<_, io::Error>(piece?.map(|piece| (piece, frames)))
    });
    Body::from_stream(pieces)
}

struct Frames {
    file: Take<File>,
    /// Started at the first piece, so that building the body costs nothing
    /// on the connection's thread.
    encoder: Option<zstd::stream::write::Encoder<'static, Vec<u8>>>,
    done: bool,
}

impl Frames {
    /// The next compressed piece, `None` once the frame has ended.
    fn next_piece(&mut self) -> io::Result<Option<Bytes>> {
        if self.done {
            return Ok(None);
        }
        let encoder = match &mut self.encoder {
            Some(encoder) => encoder,
            None => self
                .encoder
                .insert(zstd::stream::write::Encoder::new(Vec::new(), LEVEL)?),
        };
        let mut buf = vec![0u8; BLOB_CHUNK];
        loop {
            let n = self.file.read(&mut buf)?;
            if n == 0 {
                self.done = true;
                let encoder = self.encoder.take().expect("started above");
                return Ok(Some(encoder.finish()?.into()));
            }
            encoder.write_all(&buf[..n])?;
            let piece = std::mem::take(encoder.get_mut());
            if !piece.is_empty() {
                return Ok(Some(piece.into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom};

    use http_body_util::BodyExt;

    use super::*;

    /// A frame made of several pieces decodes to exactly the range asked
    /// for, and is smaller than it.
    #[tokio::test]
    async fn a_blob_frame_decodes_to_its_range() {
        let content: Vec<u8> = (0..600_000u32)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&content).unwrap();
        file.seek(SeekFrom::Start(1000)).unwrap();
        let body = blob_frame(file, 500_000);
        let frame = body.collect().await.unwrap().to_bytes();
        assert!(frame.len() < 500_000 / 3, "{} bytes", frame.len());
        let decoded = zstd::stream::decode_all(&frame[..]).unwrap();
        assert_eq!(decoded, &content[1000..501_000]);
    }
}
