use crate::{
    error::{Error, Result},
    playlist::Range,
    progress::{Progress, ProgressCallback},
};
use log::{debug, trace};
use reqwest::{Client, Url, header};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::{
    fs::{self, File, OpenOptions},
    io::AsyncWriteExt,
    sync::mpsc,
    task::JoinSet,
};

pub const CHUNK_SIZE: u64 = 1024 * 1024 * 5; // 5 MiB

/// A multi-threaded, resume-capable file downloader.
///
/// `FileDownloader` splits a file into multiple chunks and downloads them concurrently using
/// HTTP Range requests. Downloaded chunks are written to the target file sequentially
/// to ensure data integrity. If the remote server does not support HTTP Range requests,
/// the downloader falls back to streaming the file sequentially.
pub struct FileDownloader {
    /// The size of each chunk to download, in bytes.
    chunk_size: u64,
    /// The HTTP client used for downloading.
    client: Client,
    /// Optional progress tracking callback.
    progress: Option<Arc<dyn ProgressCallback>>,
    /// Flag indicating whether to attempt resuming a partial download.
    resume: bool,
    /// The number of retry attempts allowed per chunk.
    retries: u8,
    /// The number of concurrent download tasks.
    threads: u8,
}

impl FileDownloader {
    /// Creates a new [`FileDownloader`] with defaults.
    pub fn new(client: &Client) -> Self {
        Self {
            chunk_size: CHUNK_SIZE,
            client: client.clone(),
            progress: None,
            resume: true,
            retries: 10,
            threads: 5,
        }
    }

    /// Sets the size of each download chunk in bytes (default: `5 MiB`).
    pub fn chunk_size(mut self, chunk_size: u64) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Sets a progress callback to receive progress updates during download.
    pub fn progress(mut self, progress: Arc<dyn ProgressCallback>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Sets whether to attempt resuming a previous download if the output file already exists (default: `true`).
    pub fn resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }

    /// Sets the maximum number of retries per chunk on download failures (default: `10`).
    pub fn retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Sets the number of concurrent download threads (default: `5`, clamped between 1 and 16).
    pub fn threads(mut self, threads: u8) -> Self {
        self.threads = threads.clamp(1, 16);
        self
    }

    /// Downloads the file from the given URL to the target path.
    ///
    /// It queries the server via a HEAD request to check for range support and the total content length.
    /// - If ranges are supported and resume is enabled, it resumes from the existing file size.
    /// - If ranges are not supported, it falls back to a streaming sequential download.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The URL is invalid.
    /// - The request fails.
    /// - The server returns content length 0.
    /// - File I/O operations fail.
    pub async fn download(self, url: &str, output: impl AsRef<Path>) -> Result<()> {
        let url = url.parse::<Url>()?;
        let output = output.as_ref();

        debug!("Fetching {} (file@head)", url);
        let response = self.client.head(url.clone()).send().await?;
        let status = response.status();

        if !status.is_success() {
            return Err(Error::RequestFailed {
                url: url.to_string(),
                status,
                body: "HEAD".to_owned(),
            });
        }

        let content_length = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if content_length == 0 {
            bail!("Server returned content-length 0 for {}.", url);
        }

        let accepts_ranges = response
            .headers()
            .get(header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v != "none");

        let bytes_written = if self.resume {
            fs::metadata(output).await.map(|x| x.len()).unwrap_or(0)
        } else {
            if output.exists() {
                fs::remove_file(output).await?;
            }
            0
        };

        if bytes_written >= content_length {
            debug!("{} is already downloaded.", output.to_string_lossy());
            return Ok(());
        }

        if !accepts_ranges {
            debug!("Server does not support range requests, falling back to streaming download.");
            let progress = Progress::new("dl", 1, self.progress.clone());
            let progress_handle = progress.spawn();
            let mut request = self.client.get(url.clone());

            if bytes_written > 0 {
                request = request.header(header::RANGE, format!("bytes={}-", bytes_written));
            }

            let mut response = request.send().await?;

            if !response.status().is_success() {
                return Err(Error::RequestFailed {
                    url: url.to_string(),
                    status: response.status(),
                    body: response.text().await?,
                });
            }

            let mut file = if bytes_written > 0 {
                OpenOptions::new().append(true).open(output).await?
            } else {
                File::create(output).await?
            };

            while let Some(bytes) = response.chunk().await? {
                file.write_all(&bytes).await?;
            }

            file.flush().await?;
            progress.update(content_length as usize);
            progress_handle.abort();
            progress.finish();

            return Ok(());
        }

        let all_chunks = Self::compute_chunks(0, content_length, self.chunk_size);
        let chunks = Self::compute_chunks(bytes_written, content_length, self.chunk_size);
        let total_chunks = chunks.len();
        let skipped_chunks = all_chunks.len() - total_chunks;

        let progress = Progress::new("dl", all_chunks.len(), self.progress);

        for (start, end) in &all_chunks[..skipped_chunks] {
            progress.skip((end - start + 1) as usize);
        }
        let progress_handle = progress.spawn();

        let (tx, rx) = mpsc::channel(self.threads as usize * 2);

        let writer_output = output.to_owned();
        let writer_progress = progress.clone();
        let writer_handle = tokio::spawn(async move {
            Self::sequential_writer(
                rx,
                &writer_output,
                bytes_written,
                total_chunks,
                writer_progress,
            )
            .await
        });

        let mut set: JoinSet<Result<()>> = JoinSet::new();

        for (idx, (start, end)) in chunks.into_iter().enumerate() {
            while set.len() >= self.threads as usize {
                if let Some(Ok(result)) = set.join_next().await {
                    result?;
                }
            }

            let client = self.client.clone();
            let retries = self.retries;
            let tx = tx.clone();
            let url = url.clone();

            set.spawn(async move {
                let bytes = Self::download_chunk(&client, &url, Range(start, end), retries).await?;
                tx.send((idx, bytes))
                    .await
                    .map_err(|_| Error::Other("File writer channel closed.".into()))?;
                Ok(())
            });
        }

        while let Some(Ok(result)) = set.join_next().await {
            result?;
        }

        drop(tx);
        writer_handle.await??;
        progress_handle.abort();
        progress.finish();

        Ok(())
    }

    fn compute_chunks(offset: u64, total: u64, chunk_size: u64) -> Vec<(u64, u64)> {
        let mut chunks = Vec::new();
        let mut start = offset;

        while start < total {
            let end = (start + chunk_size - 1).min(total - 1);
            chunks.push((start, end));
            start = end + 1;
        }

        chunks
    }

    async fn download_chunk(
        client: &Client,
        url: &Url,
        range: Range,
        max_retries: u8,
    ) -> Result<Vec<u8>> {
        let range_label = format!("{}-{}", range.0, range.1);
        trace!("Fetching {} (file@{})", url, range_label);
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                trace!("ReFetching {} (file@{})", url, range_label);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            match client
                .get(url.clone())
                .header(header::RANGE, &range)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();

                    if status == reqwest::StatusCode::PARTIAL_CONTENT {
                        let bytes = response.bytes().await?.to_vec();
                        let expected_len = range
                            .1
                            .checked_sub(range.0)
                            .and_then(|length| length.checked_add(1));

                        if expected_len == Some(bytes.len() as u64) {
                            return Ok(bytes);
                        }

                        last_err = Some(Error::Other(format!(
                            "Invalid byte-range response for {} ({}): expected {} bytes, received {}.",
                            url,
                            range_label,
                            expected_len.map_or_else(|| "an unrepresentable number of".to_owned(), |n| n.to_string()),
                            bytes.len()
                        )));
                    } else {
                        last_err = Some(Error::RequestFailed {
                            url: url.to_string(),
                            status,
                            body: format!(
                                "Expected HTTP 206 Partial Content for range {}, got {}.",
                                range_label, status
                            ),
                        });
                    }
                }
                Err(e) => {
                    last_err = Some(Error::RequestFailed {
                        url: url.to_string(),
                        status: e.status().unwrap_or_default(),
                        body: format!("GET range {}", range_label),
                    });
                }
            }
        }

        Err(last_err.unwrap_or(Error::Other(format!(
            "{} download failed after max retries.",
            url
        ))))
    }

    async fn sequential_writer(
        mut rx: mpsc::Receiver<(usize, Vec<u8>)>,
        output: &Path,
        bytes_written: u64,
        total_chunks: usize,
        progress: Progress,
    ) -> Result<()> {
        let mut file = if bytes_written > 0 {
            OpenOptions::new().append(true).open(output).await?
        } else {
            File::create(output).await?
        };

        let mut next_idx = 0;
        let mut pending: BTreeMap<usize, Vec<u8>> = BTreeMap::new();

        while let Some((idx, data)) = rx.recv().await {
            if idx == next_idx {
                // This is the next expected chunk — write directly.
                let size = data.len();
                file.write_all(&data).await?;
                progress.update(size);
                next_idx += 1;

                // Flush any consecutive buffered chunks.
                while let Some(buffered) = pending.remove(&next_idx) {
                    let size = buffered.len();
                    file.write_all(&buffered).await?;
                    progress.update(size);
                    next_idx += 1;
                }

                file.flush().await?;
            } else {
                // Out of order — buffer it.
                pending.insert(idx, data);
            }
        }

        if total_chunks > next_idx {
            bail!(
                "Download incomplete, received only {}/{} chunks.",
                next_idx,
                total_chunks,
            );
        }

        file.flush().await?;
        Ok(())
    }
}
