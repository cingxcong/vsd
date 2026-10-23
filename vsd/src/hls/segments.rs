use crate::{
    error::{Error, Result},
    playlist,
};

fn parse_byte_range(br: &m3u8_rs::ByteRange, next_byte: &mut u64) -> Result<playlist::Range> {
    if br.length == 0 {
        return Err(Error::Other("HLS byte-range length must be greater than zero.".into()));
    }

    let start = br.offset.unwrap_or(*next_byte);
    let end = start
        .checked_add(br.length)
        .and_then(|exclusive_end| exclusive_end.checked_sub(1))
        .ok_or_else(|| Error::Other("HLS byte-range end overflows the supported range.".into()))?;
    *next_byte = end
        .checked_add(1)
        .ok_or_else(|| Error::Other("HLS byte-range continuation overflows the supported range.".into()))?;

    Ok(playlist::Range(start, end))
}

fn parse_key(key: m3u8_rs::Key) -> playlist::Key {
    let mut method = match &key.method {
        m3u8_rs::KeyMethod::AES128 => playlist::KeyMethod::Aes128,
        m3u8_rs::KeyMethod::None => playlist::KeyMethod::None,
        m3u8_rs::KeyMethod::SampleAES => playlist::KeyMethod::SampleAes,
        m3u8_rs::KeyMethod::Other(x) if x == "SAMPLE-AES-CENC" || x == "SAMPLE-AES-CTR" => {
            playlist::KeyMethod::Cenc
        }
        m3u8_rs::KeyMethod::Other(x) => playlist::KeyMethod::Other(x.to_owned()),
    };

    if method != playlist::KeyMethod::Cenc
        && let Some(keyformat) = key.keyformat.as_deref()
        && matches!(
            keyformat,
            "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"
                | "urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95"
                | "com.apple.streamingkeydelivery"
        )
    {
        method = playlist::KeyMethod::Cenc;
    }

    if method != playlist::KeyMethod::Cenc
        && let Some(uri) = key.uri.as_ref()
        && (uri.starts_with("data:text/plain;base64,") || uri.starts_with("skd://"))
    {
        method = playlist::KeyMethod::Cenc;
    }

    playlist::Key {
        default_kid: None,
        iv: key.iv,
        method,
        uri: key.uri,
    }
}

pub fn push_segments(stream: &mut playlist::MediaPlaylist, m3u8: m3u8_rs::MediaPlaylist) -> Result<()> {
    stream.i_frame = m3u8.i_frames_only;
    stream.live = !m3u8.end_list;
    stream.media_sequence = m3u8.media_sequence;

    let mut next_segment_byte = 0;
    let mut previous_segment_uri: Option<String> = None;
    let mut next_map_byte = 0;
    let mut previous_map_uri: Option<String> = None;

    for segment in m3u8.segments {
        if previous_segment_uri.as_deref() != Some(segment.uri.as_str()) {
            next_segment_byte = 0;
            previous_segment_uri = Some(segment.uri.clone());
        }

        let map = segment
            .map
            .map(|x| {
                if previous_map_uri.as_deref() != Some(x.uri.as_str()) {
                    next_map_byte = 0;
                    previous_map_uri = Some(x.uri.clone());
                }
                Ok::<playlist::Map, Error>(playlist::Map {
                    uri: x.uri,
                    range: x
                        .byte_range
                        .as_ref()
                        .map(|br| parse_byte_range(br, &mut next_map_byte))
                        .transpose()?,
                })
            })
            .transpose()?;

        let range = segment
            .byte_range
            .as_ref()
            .map(|br| parse_byte_range(br, &mut next_segment_byte))
            .transpose()?;

        stream.segments.push(playlist::Segment {
            duration: segment.duration,
            key: segment.key.map(parse_key),
            map,
            range,
            uri: segment.uri,
        });
    }

    if let Some(first) = stream.segments.first() {
        let is_mp4 = |uri: &str| {
            let path = uri.split_once('?').map_or(uri, |(p, _)| p);
            path.ends_with(".mp4") || path.ends_with(".m4s")
        };

        if is_mp4(&first.uri) || first.map.as_ref().is_some_and(|m| is_mp4(&m.uri)) {
            stream.extension = Some("mp4".to_owned());
        }
    }

    Ok(())
}
