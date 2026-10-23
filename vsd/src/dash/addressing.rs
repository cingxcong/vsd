use crate::{
    PlaylistDownloadConfig,
    dash::{Template, parse_range},
    error::{Error, Result},
    playlist::{Map, Range, Segment},
    utils,
};
use dash_mpd::SegmentTemplate;
use log::debug;
use reqwest::{Url, header};
use vsd_mp4::boxes::SidxBox;

pub fn parse_init(
    initialization: &dash_mpd::Initialization,
    base_url: &Url,
    template: &Template,
) -> Result<Map> {
    Ok(Map {
        range: parse_range(&initialization.range),
        uri: if let Some(source_url) = &initialization.sourceURL {
            base_url.join(&template.resolve(source_url))?.to_string()
        } else {
            base_url.to_string()
        },
    })
}

pub fn resolve_segment_list(
    segment_list: &dash_mpd::SegmentList,
    base_url: &Url,
    template: &Template,
    has_base_url: bool,
) -> Result<Vec<Segment>> {
    let mut segments = Vec::new();

    for segment_url in &segment_list.segment_urls {
        // We are ignoring SegmentURL@indexRange
        let byte_range = parse_range(&segment_url.mediaRange);

        if let Some(media) = &segment_url.media {
            segments.push(Segment {
                range: byte_range,
                uri: base_url.join(media)?.to_string(),
                ..Default::default()
            });
        } else if has_base_url {
            segments.push(Segment {
                range: byte_range,
                uri: base_url.to_string(),
                ..Default::default()
            });
        }
    }

    if let (Some(first), Some(init)) = (segments.first_mut(), &segment_list.Initialization) {
        first.map = Some(parse_init(init, base_url, template)?);
    }

    Ok(segments)
}

pub fn resolve_segment_template_init(
    rt: Option<&SegmentTemplate>,
    at: Option<&SegmentTemplate>,
    base_url: &Url,
    template: &Template,
) -> Result<Option<Map>> {
    // Try @initialization attribute
    let tmpl_init = rt
        .and_then(|t| t.initialization.clone())
        .or(at.and_then(|t| t.initialization.clone()));

    if let Some(init) = tmpl_init {
        return Ok(Some(Map {
            range: None,
            uri: base_url.join(&template.resolve(&init))?.to_string(),
        }));
    }

    // Try <Initialization> child element
    let tmpl_init = rt
        .and_then(|t| t.Initialization.as_ref())
        .or(at.and_then(|t| t.Initialization.as_ref()));

    if let Some(init) = tmpl_init {
        return Ok(Some(parse_init(init, base_url, template)?));
    }

    Ok(None)
}

pub fn resolve_segment_timeline(
    segment_timeline: &dash_mpd::SegmentTimeline,
    base_url: &Url,
    template: &mut Template,
    period_duration_secs: f64,
    media: &str,
    start_number: u64,
    timescale: u64,
) -> Result<Vec<Segment>> {
    const MAX_SEGMENTS: usize = 1_000_000;

    if timescale == 0 {
        return Err(Error::DashParse("SegmentTemplate @timescale must be greater than zero.".into()));
    }
    if !period_duration_secs.is_finite() || period_duration_secs < 0.0 {
        return Err(Error::DashParse("Period duration must be finite and non-negative.".into()));
    }

    let media = template.resolve(media);
    let scale = timescale as f64;
    let end_time = period_duration_secs * scale;
    let mut number = start_number;
    let mut segments = Vec::new();
    let mut segment_time: u64 = 0;

    for (index, s) in segment_timeline.segments.iter().enumerate() {
        if s.d == 0 {
            return Err(Error::DashParse("SegmentTimeline @d must be greater than zero.".into()));
        }
        if let Some(t) = s.t {
            segment_time = t;
        }

        let repeat_boundary = if s.r < 0 {
            segment_timeline.segments[index + 1..]
                .iter()
                .find_map(|next| next.t)
                .map(|t| t as f64)
                .unwrap_or(end_time)
        } else {
            0.0
        };

        let mut repeat_index: i64 = 0;
        loop {
            if segments.len() >= MAX_SEGMENTS {
                return Err(Error::DashParse(format!(
                    "SegmentTimeline exceeds the safety limit of {MAX_SEGMENTS} segments."
                )));
            }

            if s.r < 0 && repeat_index > 0 && segment_time as f64 >= repeat_boundary {
                break;
            }

            template.insert("Time", segment_time);
            template.insert("Number", number);
            segments.push(Segment {
                duration: (s.d as f64 / scale) as f32,
                uri: base_url.join(&template.resolve(&media))?.to_string(),
                ..Default::default()
            });

            number = number.checked_add(1).ok_or_else(|| {
                Error::DashParse("Segment number overflows the supported range.".into())
            })?;

            if s.r >= 0 {
                if repeat_index >= s.r as i64 {
                    break;
                }
            } else {
                let next_time = segment_time.checked_add(s.d).ok_or_else(|| {
                    Error::DashParse("SegmentTimeline timestamp overflows the supported range.".into())
                })?;
                if next_time as f64 >= repeat_boundary {
                    segment_time = next_time;
                    break;
                }
            }

            segment_time = segment_time.checked_add(s.d).ok_or_else(|| {
                Error::DashParse("SegmentTimeline timestamp overflows the supported range.".into())
            })?;
            repeat_index += 1;
        }

        // Advance to the start time after the final segment in this S element.
        segment_time = segment_time.checked_add(s.d).ok_or_else(|| {
            Error::DashParse("SegmentTimeline timestamp overflows the supported range.".into())
        })?;
    }

    Ok(segments)
}

pub fn resolve_segment_template_duration(
    base_url: &Url,
    template: &mut Template,
    period_duration_secs: f64,
    duration: f64,
    media: &str,
    start_number: u64,
    timescale: u64,
) -> Result<Vec<Segment>> {
    const MAX_SEGMENTS: usize = 1_000_000;

    if timescale == 0 {
        return Err(Error::DashParse("SegmentTemplate @timescale must be greater than zero.".into()));
    }
    if !duration.is_finite() || duration <= 0.0 {
        return Err(Error::DashParse("SegmentTemplate @duration must be finite and greater than zero.".into()));
    }
    if !period_duration_secs.is_finite() || period_duration_secs < 0.0 {
        return Err(Error::DashParse("Period duration must be finite and non-negative.".into()));
    }

    let media = template.resolve(media);
    let segment_duration = duration / timescale as f64;
    let count = (period_duration_secs / segment_duration).ceil();
    if !count.is_finite() || count < 0.0 || count > MAX_SEGMENTS as f64 {
        return Err(Error::DashParse(format!(
            "SegmentTemplate resolves to an invalid or excessive segment count (maximum {MAX_SEGMENTS})."
        )));
    }
    let segment_count = count as usize;
    let mut segments = Vec::with_capacity(segment_count);
    let mut number = start_number;

    for _ in 0..segment_count {
        template.insert("Number", number);
        segments.push(Segment {
            duration: segment_duration as f32,
            uri: base_url.join(&template.resolve(&media))?.to_string(),
            ..Default::default()
        });
        number = number.checked_add(1).ok_or_else(|| {
            Error::DashParse("Segment number overflows the supported range.".into())
        })?;
    }

    Ok(segments)
}

pub async fn resolve_segment_base(
    segment_base: &dash_mpd::SegmentBase,
    base_url: &Url,
    template: &Template,
    config: &PlaylistDownloadConfig,
) -> Result<Vec<Segment>> {
    let mut segments = Vec::new();

    if let Some(index_range) = parse_range(&segment_base.indexRange) {
        debug!(
            "Fetching {} (sidx@{}-{})",
            base_url, index_range.0, index_range.1
        );
        let request = config
            .client
            .get(base_url.clone())
            .query(&*config.query)
            .header(header::RANGE, &index_range);
        let response = request.send().await?;
        let bytes = utils::fetch_bytes(response).await?;
        let Some(sidx) = SidxBox::from_init(&bytes, index_range.0)? else {
            return Err(Error::DashParse(
                "Missing sidx box in initialization.".into(),
            ));
        };

        for range in sidx.ranges {
            segments.push(Segment {
                range: Some(Range(range.start, range.end)),
                uri: base_url.to_string(),
                ..Default::default()
            });
        }

        if let (Some(first), Some(init)) = (segments.first_mut(), &segment_base.Initialization) {
            let mut map = parse_init(init, base_url, template)?;
            map.range = Some(Range(0, index_range.1));
            first.map = Some(map);
        }
    } else {
        segments.push(Segment {
            map: segment_base
                .Initialization
                .as_ref()
                .map(|init| parse_init(init, base_url, template))
                .transpose()?,
            uri: base_url.to_string(),
            ..Default::default()
        });
    }

    Ok(segments)
}
