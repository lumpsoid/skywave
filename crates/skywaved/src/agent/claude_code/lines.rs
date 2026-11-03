//! Splits the CLI's stdout into lines that survive bad bytes.
//!
//! `AsyncBufReadExt::lines` fails on invalid UTF-8 and buffers a line of any
//! length; either would end or swamp a session over one bad line. Here a
//! line is decoded lossily, and a line longer than the cap is dropped as it
//! streams past, so only a real read error or EOF ends the output.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The longest stdout line kept. Generous: a tool result or an image the
/// CLI echoes back fits on one line.
pub(super) const MAX_LINE: usize = 32 * 1024 * 1024;

/// One line of stdout, without its newline.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Line {
    /// Invalid UTF-8 replaced by U+FFFD.
    Text(String),
    /// A line over the cap, dropped; how many bytes it had.
    TooLong(usize),
}

/// Reads [`Line`]s from `R`, holding at most `max` bytes of one line.
pub(super) struct LineReader<R> {
    inner: R,
    max: usize,
    /// The current line so far, while within `max`.
    buf: Vec<u8>,
    /// The current line's length once it went over `max`.
    dropped: Option<usize>,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub(super) fn new(inner: R, max: usize) -> LineReader<R> {
        LineReader {
            inner,
            max,
            buf: Vec::new(),
            dropped: None,
        }
    }

    /// The next line; `None` at EOF. A last line without a newline still
    /// counts. Cancel safe: a partial line stays buffered for the next call.
    pub(super) async fn next_line(&mut self) -> std::io::Result<Option<Line>> {
        loop {
            // The only await: nothing is consumed until it returns.
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                if self.buf.is_empty() && self.dropped.is_none() {
                    return Ok(None);
                }
                return Ok(Some(self.take()));
            }
            let newline = available.iter().position(|&b| b == b'\n');
            let chunk = &available[..newline.unwrap_or(available.len())];
            match &mut self.dropped {
                Some(len) => *len += chunk.len(),
                None if self.buf.len() + chunk.len() > self.max => {
                    self.dropped = Some(self.buf.len() + chunk.len());
                    self.buf = Vec::new();
                }
                None => self.buf.extend_from_slice(chunk),
            }
            let used = newline.map_or(chunk.len(), |at| at + 1);
            self.inner.consume(used);
            if newline.is_some() {
                return Ok(Some(self.take()));
            }
        }
    }

    fn take(&mut self) -> Line {
        match self.dropped.take() {
            Some(len) => Line::TooLong(len),
            None => {
                let line = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                Line::Text(line)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_all(input: &[u8], max: usize) -> Vec<Line> {
        // A small buffer, so lines span several fills.
        let mut reader = LineReader::new(tokio::io::BufReader::with_capacity(4, input), max);
        let mut lines = Vec::new();
        while let Some(line) = reader.next_line().await.unwrap() {
            lines.push(line);
        }
        lines
    }

    fn text(line: &str) -> Line {
        Line::Text(line.to_owned())
    }

    #[tokio::test]
    async fn invalid_utf8_is_replaced_and_reading_goes_on() {
        let lines = read_all(b"{\"a\":1}\nbad \xff\xfe byte\n{\"b\":2}\n", 64).await;
        assert_eq!(
            lines,
            [
                text("{\"a\":1}"),
                text("bad \u{fffd}\u{fffd} byte"),
                text("{\"b\":2}")
            ]
        );
    }

    #[tokio::test]
    async fn an_over_long_line_is_dropped_and_reading_goes_on() {
        let lines = read_all(b"short\n0123456789abcdef\nexactly-10\nnext\n", 10).await;
        assert_eq!(
            lines,
            [
                text("short"),
                Line::TooLong(16),
                text("exactly-10"),
                text("next")
            ]
        );
    }

    #[tokio::test]
    async fn a_last_line_without_a_newline_counts() {
        assert_eq!(read_all(b"a\nb", 10).await, [text("a"), text("b")]);
        assert_eq!(
            read_all(b"a\n0123456789abc", 10).await,
            [text("a"), Line::TooLong(13)]
        );
        assert_eq!(read_all(b"", 10).await, []);
    }
}
