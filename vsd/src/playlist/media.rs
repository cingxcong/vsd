use crate::{
    core::{self, PlaylistDownloadConfig, Stream},
    error::{Error, Result},
    playlist::types::{MediaPlaylist, MediaType, PlaylistType, Segment},
    progress::{Progress, ProgressCallback},
    utils,
};
use log::debug;
use reqwest::header;
use std::{fmt::Display, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use url::Url;

impl MediaPlaylist {
    /// Resolves the absolute path to the local output file for this stream.
    pub(crate) fn path(&self, directory: Option<&PathBuf>) -> PathBuf {
        let filename = format!("vsd-{}-{}.{}", self.media_type, self.id, self.extension());
        directory
            .map(|d| d.join(&filename))
            .unwrap_or_else(|| PathBuf::from(filename))
    }

    /// Extracts the default key ID (KID) in hexadecimal format if the stream is encrypted.
    pub fn default_kid(&self) -> Option<String> {
        self.segments
            .first()
            .and_then(|s| s.key.as_ref())
            .and_then(|k| k.default_kid.as_ref())
            .map(|kid| kid.to_ascii_lowercase().replace('-', ""))
    }

    /// Determines the file extension of the media segments.
    ///
    /// Checks segment URIs, map URIs, and falls back to protocol defaults (`ts` for HLS, `mp4` for DASH).
    pub fn extension(&self) -> &str {
        if let Some(ext) = &self.extension {
            return ext;
        }

        if let Some(first) = self.segments.first() {
            let is_mp4 = |uri: &str| {
                let path = uri.split_once('?').map_or(uri, |(p, _)| p);
                path.ends_with(".mp4") || path.ends_with(".m4s")
            };

            if is_mp4(&first.uri) || first.map.as_ref().is_some_and(|m| is_mp4(&m.uri)) {
                return "mp4";
            }
        }

        match self.playlist_type {
            PlaylistType::Hls => "ts",
            PlaylistType::Dash => "mp4",
        }
    }

    /// Downloads the media playlist segments.
    ///
    /// Spawns a progress bar updates callback, matches the media type (video/audio vs subtitles),
    /// and delegates segment downloading to the core downloader modules.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - [`Error::MissingSegments`] if the segment list is empty.
    /// - [`Error::UnsupportedEncryption`] if the stream uses an unsupported encryption format.
    /// - [`Error::MissingKey`] if a decryption key is required but missing.
    /// - [`Error::DownloadInterrupted`] if the download is cancelled via the cancellation token.
    /// - Other connection, disk I/O, or decryption errors propagated from underlying tasks.
    pub async fn download(
        &self,
        config: &PlaylistDownloadConfig,
        progress: Arc<dyn ProgressCallback>,
        token: &CancellationToken,
    ) -> Result<Stream> {
        if self.segments.is_empty() {
            return Err(Error::MissingSegments);
        }

        let progress = Progress::new(&self.id, self.segments.len(), Some(progress));
        let temp_file = if self.media_type == MediaType::Subtitles {
            core::sub::download(config, progress, token, self).await?
        } else {
            core::vid::download(config, progress, token, self).await?
        };

        Ok(temp_file)
    }

    /// Fetches the initialization segment (typically fMP4 headers) if the stream requires one.
    ///
    /// # Errors
    ///
    /// Returns an error if fetching or downloading the init segment fails.
    pub async fn fetch_init(&self, config: &PlaylistDownloadConfig) -> Result<Option<Vec<u8>>> {
        let Some(Segment { map: Some(map), .. }) = self.segments.first() else {
            return Ok(None);
        };

        let url = self.uri.parse::<Url>()?.join(&map.uri)?;
        let mut request = config.client.get(url.clone()).query(&*config.query);

        if let Some(range) = &map.range {
            request = request.header(header::RANGE, range);
        }

        debug!(
            "Fetching {} (init@{})",
            url,
            map.range
                .as_ref()
                .map(|x| format!("{}-{}", x.0, x.1))
                .as_deref()
                .unwrap_or("full-range")
        );
        let response = request.send().await?;
        let bytes = utils::fetch_bytes(response).await?;
        Ok(Some(bytes))
    }
}

impl Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Video => "vid",
                Self::Audio => "aud",
                Self::Subtitles => "sub",
                Self::Undefined => "und",
            }
        )
    }
}

impl MediaPlaylist {
    fn fmt_bandwidth(&self) -> String {
        self.bandwidth
            .and_then(|b| {
                let b = b / 1000;
                if b > 0 { Some(format!("{}k", b)) } else { None }
            })
            .unwrap_or("?".to_owned())
    }

    fn fmt_codecs(&self) -> String {
        self.codecs
            .as_ref()
            .map(|c| {
                truncate_chars(c, 11)
            })
            .unwrap_or("?".to_owned())
    }

    fn fmt_language(&self) -> String {
        self.language
            .as_ref()
            .map(|c| {
                truncate_chars(c, 9)
            })
            .unwrap_or("?".to_owned())
    }

    /// Returns a formatted string representation of the media playlist suitable for printing in console logs or stream listings.
    pub fn display(&self) -> String {
        self.to_string()
            .split('|')
            .map(|x| x.replace(" ", ""))
            .collect::<Vec<String>>()
            .join(" ")
    }
}

// Truncate by Unicode scalar values rather than byte offsets: codec and language
// metadata can contain non-ASCII text, and slicing at an arbitrary byte can panic.
fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

impl std::fmt::Display for MediaPlaylist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.media_type {
            MediaType::Video => {
                write!(
                    f,
                    "{:>9} | {:>5} | {:>12} | {:>3} fps",
                    self.resolution
                        .map(|(w, h)| format!("{}x{}", w, h))
                        .unwrap_or_default(),
                    self.fmt_bandwidth(),
                    self.fmt_codecs(),
                    self.frame_rate
                        .map(|f| format!("{:.0}", f))
                        .as_deref()
                        .unwrap_or("?")
                )?;
                if self.live {
                    write!(f, " | live")?;
                }
            }
            MediaType::Audio => {
                write!(
                    f,
                    "{:>9} | {:>5} | {:>12} | {:>3} ch",
                    self.fmt_language(),
                    self.fmt_bandwidth(),
                    self.fmt_codecs(),
                    self.channels
                        .map(|c| c.to_string())
                        .as_deref()
                        .unwrap_or("?")
                )?;
                if self.live {
                    write!(f, " | live")?;
                }
            }
            MediaType::Subtitles => {
                write!(
                    f,
                    "{:>9} | {:>5} | {:>12} |",
                    self.fmt_language(),
                    "?k",
                    self.fmt_codecs()
                )?;
                if self.live {
                    write!(f, " | live")?;
                }
            }
            MediaType::Undefined => {
                write!(f, "{}", self.uri)?;
            }
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn truncate_chars_handles_multibyte_text() {
        assert_eq!(truncate_chars("en-US", 9), "en-US");
        assert_eq!(truncate_chars("日本語の字幕", 4), "日本語の…");
        assert_eq!(truncate_chars("éééé", 2), "éé…");
    }
}
