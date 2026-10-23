use crate::{
    PlaylistDownloadConfig, dash,
    error::{Error, Result},
    format::{FormatExpr, SelectType},
    hls,
    playlist::{MasterPlaylist, MediaPlaylist, PlaylistType},
    utils,
};
use base64::Engine;
use log::debug;
use reqwest::{Url, header};
use std::path::Path;
use tokio::fs;

pub async fn playlist(
    config: &PlaylistDownloadConfig,
    base_url: &Option<Url>,
    uri: &str,
) -> Result<FetchedPlaylist> {
    let path = Path::new(uri);
    let mut typ = None;

    if path.exists() {
        let Some(base_url) = base_url else {
            bail!("--baseurl flag is required for local playlist file.");
        };

        if let Some(ext) = path.extension() {
            if ext == "mpd" {
                typ = Some(PlaylistType::Dash)
            } else if ext == "m3u" || ext == "m3u8" {
                typ = Some(PlaylistType::Hls)
            }
        }

        Ok(FetchedPlaylist {
            url: base_url.to_owned(),
            data: fs::read(path).await?,
            typ,
        })
    } else if let Ok(input) = uri.parse::<Url>() {
        debug!("Fetching {} (playlist)", input);
        let response = config
            .client
            .get(input)
            .query(&*config.query)
            .send()
            .await?;

        if let Some(content_type) = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|x| x.to_str().ok())
        {
            if content_type == "application/dash+xml" || content_type == "video/vnd.mpeg.dash.mpd" {
                typ = Some(PlaylistType::Dash)
            } else if content_type == "application/x-mpegurl"
                || content_type == "application/vnd.apple.mpegurl"
            {
                typ = Some(PlaylistType::Hls)
            }
        }

        Ok(FetchedPlaylist {
            url: response.url().to_owned(),
            data: utils::fetch_bytes(response).await?,
            typ,
        })
    } else {
        bail!("Unable to determine playlist type.");
    }
}

pub struct FetchedPlaylist {
    url: Url,
    data: Vec<u8>,
    typ: Option<PlaylistType>,
}

impl FetchedPlaylist {
    fn playlist_type(&self) -> Result<PlaylistType> {
        if let Some(typ) = &self.typ {
            return Ok(typ.to_owned());
        }
        if self.data.windows(7).any(|w| w == b"#EXTM3U") {
            return Ok(PlaylistType::Hls);
        }
        if self.data.windows(4).any(|w| w == b"<MPD") {
            return Ok(PlaylistType::Dash);
        }
        bail!("Unable to determine playlist type.");
    }

    pub async fn parse(
        &self,
        config: &PlaylistDownloadConfig,
        format_expr: &FormatExpr,
        select_type: &SelectType,
        partial_parse: bool,
    ) -> Result<MasterPlaylist> {
        match self.playlist_type()? {
            PlaylistType::Dash => {
                let xml = String::from_utf8_lossy(&self.data);
                let Ok(mpd) = dash_mpd::parse(&xml) else {
                    bail!("Unable to parse dash playlist.");
                };
                let mut pl = dash::parse_as_master(&self.url, &mpd).sort_streams();

                if partial_parse {
                    pl = pl.select_streams(format_expr, select_type)?;
                }

                for stream in &mut pl.streams {
                    dash::push_segments(config, &self.url, &mpd, stream).await?;
                }

                Ok(pl)
            }
            PlaylistType::Hls => {
                match m3u8_rs::parse_playlist_res(&self.data)
                    .map_err(|_| Error::Other("Unable to parse hls playlist.".into()))?
                {
                    m3u8_rs::Playlist::MasterPlaylist(m3u8) => {
                        let mut pl = hls::parse_as_master(&self.url, &m3u8).sort_streams();

                        if partial_parse {
                            pl = pl.select_streams(format_expr, select_type)?;
                        }

                        for stream in &mut pl.streams {
                            let m3u8 = if let Some(bs) = stream
                                .uri
                                .clone()
                                .strip_prefix("data:application/x-mpegurl;base64,")
                            {
                                stream.uri = self.url.to_string();
                                base64::engine::general_purpose::STANDARD.decode(bs)?
                            } else {
                                stream.uri = self.url.join(&stream.uri)?.to_string();
                                debug!("Fetching {} (media-playlist)", stream.uri);
                                let response = config
                                    .client
                                    .get(&stream.uri)
                                    .query(&*config.query)
                                    .send()
                                    .await?;
                                utils::fetch_bytes(response).await?
                            };

                            let media_playlist =
                                m3u8_rs::parse_media_playlist_res(&m3u8).map_err(|_| {
                                    Error::Other("Unable to parse hls playlist.".into())
                                })?;
                            hls::push_segments(stream, media_playlist)?;
                        }

                        Ok(pl)
                    }
                    m3u8_rs::Playlist::MediaPlaylist(m3u8) => {
                        let mut stream = MediaPlaylist {
                            id: utils::gen_id(self.url.as_str(), ""),
                            playlist_type: PlaylistType::Hls,
                            uri: self.url.to_string(),
                            ..Default::default()
                        };
                        hls::push_segments(&mut stream, m3u8)?;
                        Ok(MasterPlaylist {
                            playlist_type: PlaylistType::Hls,
                            streams: vec![stream],
                            uri: self.url.to_string(),
                        })
                    }
                }
            }
        }
    }
}
