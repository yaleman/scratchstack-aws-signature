//! Smart request body buffering with AWS chunked encoding support.
//!
//! Buffers HTTP request bodies in memory (up to 50MB) with automatic spillover to disk,
//! and optionally decodes AWS chunked encoding for streaming uploads.

use std::io::SeekFrom;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::async_spooled_tempfile::SpooledTempFile;

use log::{debug, error, trace};

/// Decode AWS chunked encoding format
/// Format: `<hex-chunk-size>\r\n<chunk-data>\r\n...\r\n0\r\n<optional-trailers>\r\n\r\n`
fn decode_aws_chunks(data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut result = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        // Find the end of the chunk size line (look for \r\n)
        let size_line_end = data[pos..].windows(2).position(|w| w == b"\r\n").ok_or_else(|| {
            Box::new(std::io::Error::other("Invalid AWS chunk format: missing \\r\\n after chunk size"))
        })?;

        // Parse the chunk size (hex string)
        let size_str = std::str::from_utf8(&data[pos..pos + size_line_end])
            .map_err(|e| Box::new(std::io::Error::other(format!("Invalid chunk size encoding: {}", e))))?;

        let chunk_size = usize::from_str_radix(size_str, 16)
            .map_err(|e| Box::new(std::io::Error::other(format!("Invalid chunk size hex: {}", e))))?;

        // Move past the chunk size line
        pos += size_line_end + 2; // +2 for \r\n

        // If chunk size is 0, we've reached the end
        if chunk_size == 0 {
            break;
        }

        // Read the chunk data
        if pos + chunk_size > data.len() {
            return Err(Box::from(std::io::Error::other("Invalid AWS chunk: chunk size exceeds remaining data")));
        }

        result.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size;

        // Skip the trailing \r\n
        if pos + 2 > data.len() || &data[pos..pos + 2] != b"\r\n" {
            return Err(Box::from(std::io::Error::other("Invalid AWS chunk: missing \\r\\n after chunk data")));
        }
        pos += 2;
    }

    Ok(result)
}

/// A buffered request body that can be stored in memory or spilled to disk
pub struct BufferedBody {
    file: SpooledTempFile,
    size: usize,
}

impl BufferedBody {
    /// Buffer a hyper Incoming body, spilling to disk if it exceeds the threshold
    /// If should_decode_aws_chunks is true, will decode AWS chunked encoding format
    pub async fn from_incoming(
        body: Incoming,
        should_decode_aws_chunks: bool,
        max_in_memory: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut body = body;

        let mut written_bytes = 0;
        let mut file = SpooledTempFile::new(max_in_memory);
        let mut raw_data = Vec::with_capacity(1024);

        // Collect the body
        while let Some(frame) = body.frame().await {
            let frame = frame.inspect_err(|e| error!("Body read error: {}", e))?;

            if let Some(data) = frame.data_ref() {
                trace!(
                    "Body frame received: {} bytes, first 100 bytes: {:?}",
                    data.len(),
                    &data[..data.len().min(100)]
                );
                if should_decode_aws_chunks {
                    // Accumulate data for decoding
                    raw_data.extend_from_slice(data);
                } else {
                    // Write directly
                    file.write_all(data)
                        .await
                        .inspect_err(|e| error!("Failed to write to spooled temp file: {}", e))?;
                    written_bytes += data.len();
                }
            }
        }

        // If we need to decode AWS chunks, do it now
        if should_decode_aws_chunks {
            debug!("Decoding AWS chunked encoding from {} bytes", raw_data.len());
            let decoded = decode_aws_chunks(&raw_data)?;
            debug!("Decoded {} bytes from AWS chunks", decoded.len());
            file.write_all(&decoded)
                .await
                .inspect_err(|e| error!("Failed to write decoded data to spooled temp file: {}", e))?;
            written_bytes = decoded.len();
        }

        Ok(BufferedBody {
            file,
            size: written_bytes,
        })
    }

    /// Get the body as a byte vector (reading from disk if necessary)
    /// Note: This method rewinds the file to the start before AND after reading
    /// to allow multiple reads from the same buffer.
    pub async fn to_vec(&mut self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut buffer = Vec::with_capacity(self.size);
        self.file.seek(SeekFrom::Start(0)).await.inspect_err(|e| error!("Failed to rewind spooled file: {}", e))?;
        self.file.read_to_end(&mut buffer).await.inspect_err(|e| error!("Failed to read spooled file: {}", e))?;
        // Rewind again so the buffer can be read multiple times
        self.file
            .seek(SeekFrom::Start(0))
            .await
            .inspect_err(|e| error!("Failed to rewind spooled file after read: {}", e))?;
        Ok(buffer)
    }
}
