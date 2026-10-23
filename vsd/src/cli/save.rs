use crate::{
    PlaylistDownloader,
    cookie::Cookies,
    error::Result,
    playlist::{MasterPlaylist, MediaType},
};
use clap::Args;
use colored::Colorize;
use log::info;
use reqwest::{
    Client, Proxy, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::fs;

/// Download streams from DASH or HLS playlist.
#[derive(Args, Clone, Debug)]
pub struct Save {
    /// https://.. (playlist) | .m3u8 | .mpd
    #[arg(required = true)]
    pub input: String,

    /// Baseurl for resolving relative segment paths for local playlist.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<Url>,

    /// Output file path for the muxed file using ffmpeg.
    ///
    /// This will overwrite existing output file and delete downloaded streams.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Output playlist metadata as json instead of downloading streams.
    #[arg(long)]
    pub parse: bool,

    /// Force a specific subtitle codec for muxing.
    #[arg(long, value_name = "CODEC", default_value = "copy")]
    pub subs_codec: String,

    /// Cookies file path for requests (netscape cookie file).
    #[arg(long, value_name = "PATH", help_heading = "Client Options")]
    pub cookies: Option<PathBuf>,

    /// Additional headers for requests in same format as curl.
    ///
    /// This option can be used multiple times.
    #[arg(short = 'H', long = "header", value_name = "KEY:VALUE", help_heading = "Client Options", value_parser = Self::parse_header)]
    pub headers: Vec<(HeaderName, HeaderValue)>,

    /// Proxy server url (http, https, or socks).
    #[arg(long, help_heading = "Client Options", value_parser = Self::parse_proxy)]
    pub proxy: Option<Proxy>,

    /// Additional query parameters for requests.
    #[arg(long, value_name = "KEY=VALUE&…", help_heading = "Client Options")]
    pub query: Option<String>,

    /// Decryption keys for drm protected content in hex format.
    #[arg(long, value_name = "KID:KEY;…", help_heading = "Decrypt Options", default_value = "", hide_default_value = true, value_parser = Self::parse_keys)]
    pub keys: HashMap<String, String>,

    /// Disable decryption and download encrypted streams.
    #[arg(long, help_heading = "Decrypt Options")]
    pub no_decrypt: bool,

    /// Download a specific section of the stream (not accurate clipping).
    ///
    /// Accepts time values in HH:MM:SS.SS, MM:SS.SS, or SS.SS formats.
    #[arg(
        long,
        value_name = "START|START-END",
        help_heading = "Download Options"
    )]
    pub clip: Option<String>,

    /// Directory path for downloaded streams.
    #[arg(short, long, value_name = "PATH", help_heading = "Download Options")]
    pub directory: Option<PathBuf>,

    /// Disable segments merging.
    #[arg(long, help_heading = "Download Options")]
    pub no_merge: bool,

    /// Disable resume and force re-downloading.
    #[arg(long, help_heading = "Download Options")]
    pub no_resume: bool,

    /// Maximum retry attempts per segment.
    #[arg(long, help_heading = "Download Options", default_value_t = 10)]
    pub retries: u8,

    /// Maximum number of concurrent download threads (1–16).
    #[arg(short, long, help_heading = "Download Options", default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=16))]
    pub threads: u8,

    /// List available streams in a table format.
    #[arg(
        short = 'F',
        long,
        help_heading = "Format Selection Options",
        conflicts_with = "list_formats_json"
    )]
    pub list_formats: bool,

    /// List available streams metadata as json.
    #[arg(long, help_heading = "Format Selection Options")]
    pub list_formats_json: bool,

    /// Format expression for selecting streams.
    ///
    /// Visit https://clitic.github.io/vsd/usage/#format-selection for more info.
    #[arg(
        short = 'f',
        long,
        value_name = "FORMAT",
        help_heading = "Format Selection Options",
        default_value = "b+s+allund"
    )]
    pub format: String,

    /// Enable interactive stream selection menu with styled prompts.
    #[arg(
        short,
        long,
        help_heading = "Format Selection Options",
        conflicts_with = "interactive_raw"
    )]
    pub interactive: bool,

    /// Enable interactive stream selection menu with plain text prompts.
    #[arg(short = 'I', long, help_heading = "Format Selection Options")]
    pub interactive_raw: bool,
}

impl Save {
    fn parse_header(s: &str) -> Result<(HeaderName, HeaderValue)> {
        if let Some((k, v)) = s.split_once(':') {
            Ok((k.trim().parse()?, v.trim().parse()?))
        } else {
            bail!("Expected 'KEY:VALUE' but found '{}'.", s);
        }
    }

    fn parse_proxy(s: &str) -> Result<Proxy> {
        Ok(Proxy::all(s)?)
    }

    fn parse_keys(s: &str) -> Result<HashMap<String, String>> {
        let mut keys = HashMap::new();

        if s.is_empty() {
            return Ok(keys);
        }

        for pair in s.split(';') {
            if let Some((kid, key)) = pair.split_once(':') {
                let kid = kid.to_ascii_lowercase().replace('-', "");
                let key = key.to_ascii_lowercase().replace('-', "");

                if kid.len() == 32
                    && key.len() == 32
                    && kid.chars().all(|c| c.is_ascii_hexdigit())
                    && key.chars().all(|c| c.is_ascii_hexdigit())
                {
                    keys.insert(kid, key);
                } else {
                    bail!("Expected 'KID:KEY;…' but found '{}'.", s);
                }
            }
        }

        Ok(keys)
    }

    fn list_formats(mp: &MasterPlaylist) {
        info!(
            "{:>2} {:>3} {:>9} {:>5} {:>12} {:>3} {:>2}",
            "ID".yellow(),
            "TYP".yellow(),
            "RES/LANG".yellow(),
            "BR".yellow(),
            "CODEC".yellow(),
            "FPS".yellow(),
            "CH".yellow(),
        );
        info!("{}", "─".repeat(42).dimmed());

        for (i, stream) in mp.streams.iter().enumerate() {
            info!(
                "{:>2} {:>3} {:>9} {:>5} {:>12} {:>3} {:>2}",
                format!("{}", i + 1).green(),
                if stream.segments.first().is_some_and(|s| s.key.is_some()) {
                    stream.media_type.to_string().bold().red()
                } else {
                    stream.media_type.to_string().normal()
                },
                if stream.media_type == MediaType::Video {
                    stream
                        .resolution
                        .map(|(w, h)| format!("{}x{}", w, h))
                        .unwrap_or_default()
                } else {
                    stream
                        .language
                        .as_ref()
                        .map(|c| {
                            truncate_display(c, 8)
                        })
                        .unwrap_or_default()
                },
                stream
                    .bandwidth
                    .and_then(|b| {
                        let b = b / 1000;
                        if b > 0 { Some(format!("{}k", b)) } else { None }
                    })
                    .unwrap_or_default(),
                stream
                    .codecs
                    .as_ref()
                    .map(|c| {
                        truncate_display(c, 11)
                    })
                    .unwrap_or_default(),
                stream
                    .frame_rate
                    .map(|f| format!("{:.0}", f))
                    .unwrap_or_default(),
                stream.channels.map(|c| c.to_string()).unwrap_or_default()
            );
        }
    }

    pub async fn execute(self) -> Result<()> {
        let mut client = Client::builder()
            .default_headers(HeaderMap::from_iter(self.headers))
            .cookie_store(true)
            .timeout(Duration::from_secs(60));
        if let Some(path) = &self.cookies {
            let jar = Cookies::parse(&fs::read(path).await?)?.as_jar();
            client = client.cookie_provider(Arc::new(jar));
        }
        if let Some(proxy) = self.proxy {
            client = client.proxy(proxy);
        }
        let client = client.build()?;
        let mut dl = PlaylistDownloader::new(&client)
            .decrypt(!self.no_decrypt)
            .format(&self.format)?
            .keys(self.keys)
            .merge(!self.no_merge)
            .resume(!self.no_resume)
            .retries(self.retries)
            .subs_codec(self.subs_codec)
            .threads(self.threads);

        if let Some(base_url) = self.base_url {
            dl = dl.base_url(base_url);
        }
        if let Some(clip) = &self.clip {
            dl = dl.clip(clip)?;
        }
        if let Some(directory) = self.directory {
            dl = dl.directory(directory);
        }
        if let Some(output) = &self.output {
            dl = dl.output(output.clone());
        }
        if let Some(query) = self.query {
            dl = dl.query(&query);
        }
        if self.interactive {
            dl = dl.interactive(false);
        } else if self.interactive_raw {
            dl = dl.interactive(true);
        }

        if self.list_formats {
            let mp = dl.parse(&self.input, false).await?;
            Self::list_formats(&mp);
        } else if self.list_formats_json {
            let mp = dl.parse(&self.input, false).await?;
            let metadata = mp.metadata(dl.get_config()).await?;
            serde_json::to_writer(std::io::stdout(), &metadata)?;
        } else if self.parse {
            let mp = dl.parse(&self.input, false).await?;

            if let Some(output) = &self.output {
                serde_json::to_writer(std::fs::File::create(output)?, &mp)?;
            } else {
                serde_json::to_writer(std::io::stdout(), &mp)?;
            }
        } else {
            dl.download(&self.input).await?;
        }
        Ok(())
    }
}


fn truncate_display(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod display_tests {
    use super::truncate_display;

    #[test]
    fn truncation_is_unicode_safe() {
        assert_eq!(truncate_display("English", 8), "English");
        assert_eq!(truncate_display("日本語字幕", 3), "日本語…");
    }
}
