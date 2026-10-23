use crate::{
    PlaylistDownloadConfig,
    dash::{
        Template,
        addressing::{
            resolve_segment_base, resolve_segment_list, resolve_segment_template_duration,
            resolve_segment_template_init, resolve_segment_timeline,
        },
        parse_locator,
    },
    error::{Error, Result},
    playlist::{Key, KeyMethod, MediaPlaylist, Segment},
};
use dash_mpd::{AdaptationSet, MPD, Representation};
use log::debug;
use reqwest::Url;

pub async fn push_segments(
    config: &PlaylistDownloadConfig,
    base_url: &Url,
    mpd: &MPD,
    stream: &mut MediaPlaylist,
) -> Result<()> {
    let Some((a_idx, r_idx)) = parse_locator(&stream.uri) else {
        return Err(Error::DashParse(format!(
            "Used invalid dash locator: '{}'",
            stream.uri
        )));
    };

    let mut segments = Vec::new();
    let mut resolved_base_url = None;

    for period in &mpd.periods {
        let Some(adaptation_set) = period.adaptations.get(a_idx) else {
            continue;
        };
        let Some(representation) = adaptation_set.representations.get(r_idx) else {
            continue;
        };

        let (period_duration_secs, dynamic_time_offset) = if let Some(d) = period
            .duration
            .as_ref()
            .or(mpd.mediaPresentationDuration.as_ref())
        {
            (d.as_secs_f64(), 0.0)
        } else if let Some(ast) = mpd.availabilityStartTime {
            // For dynamic MPDs without explicit duration, derive from wall clock
            let now = chrono::Utc::now();
            let period_start = period
                .start
                .as_ref()
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let total_elapsed =
                ((now - ast).num_milliseconds() as f64 / 1000.0 - period_start).max(0.0);
            let window = mpd
                .timeShiftBufferDepth
                .as_ref()
                .map(|d| d.as_secs_f64())
                .unwrap_or(total_elapsed);
            let capped = total_elapsed.min(window);
            (capped, total_elapsed - capped)
        } else {
            (0.0, 0.0)
        };
        let mut base_url = base_url.clone();

        for url in [
            mpd.base_url.first().map(|x| x.base.as_ref()),
            period.BaseURL.first().map(|x| x.base.as_ref()),
            adaptation_set.BaseURL.first().map(|x| x.base.as_ref()),
            representation.BaseURL.first().map(|x| x.base.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            base_url = base_url.join(url)?;
        }

        let mut template = Template::new();
        let Some(rid) = representation.id.clone() else {
            return Err(Error::DashParse(
                "Missing @id attribute on Representation node.".into(),
            ));
        };
        template.insert("RepresentationID", rid);

        if let Some(bandwidth) = representation.bandwidth {
            template.insert("Bandwidth", bandwidth);
        }

        let mut sub_segments = resolve_segments(
            config,
            adaptation_set,
            representation,
            &base_url,
            period_duration_secs,
            dynamic_time_offset,
            &mut template,
        )
        .await?;

        if let Some(first) = sub_segments.first_mut() {
            let mut cp = representation
                .ContentProtection
                .iter()
                .chain(adaptation_set.ContentProtection.iter());

            if cp.clone().any(|c| c.value.is_some()) {
                first.key = Some(Key {
                    default_kid: cp
                        .find_map(|c| c.default_KID.clone())
                        .map(|k| k.to_ascii_lowercase().replace('-', "")),
                    method: KeyMethod::Cenc,
                    ..Default::default()
                });
            }
        }

        if resolved_base_url.is_none() {
            resolved_base_url = Some(base_url);
        }

        segments.extend(sub_segments);
    }

    if segments.is_empty() {
        return Err(Error::DashParse(
            "No usable addressing mode identified for Representation node.".into(),
        ));
    }

    stream.segments = segments;

    if let Some(base_url) = resolved_base_url {
        stream.uri = base_url.to_string();
    }

    Ok(())
}

/// Try each addressing mode in priority order and return segments.
/// Init maps are attached directly to the first segment.
///
/// Addressing modes (in order):
/// 1. Representation > SegmentList
/// 2. AdaptationSet > SegmentList
/// 3. SegmentTemplate + SegmentTimeline
/// 4. SegmentTemplate@duration
/// 5. Representation > SegmentBase
/// 6. AdaptationSet > SegmentBase
/// 7. Plain BaseURL
async fn resolve_segments(
    config: &PlaylistDownloadConfig,
    adaptation_set: &AdaptationSet,
    representation: &Representation,
    base_url: &Url,
    period_duration_secs: f64,
    dynamic_time_offset: f64,
    template: &mut Template,
) -> Result<Vec<Segment>> {
    if let Some(segment_list) = &representation.SegmentList {
        debug!("Using (1) Representation > SegmentList addressing mode.");
        return resolve_segment_list(
            segment_list,
            base_url,
            template,
            !representation.BaseURL.is_empty(),
        );
    }

    if let Some(segment_list) = &adaptation_set.SegmentList {
        debug!("Using (2) AdaptationSet > SegmentList addressing mode.");
        return resolve_segment_list(
            segment_list,
            base_url,
            template,
            !adaptation_set.BaseURL.is_empty(),
        );
    }

    let rt = representation.SegmentTemplate.as_ref();
    let at = adaptation_set.SegmentTemplate.as_ref();

    if rt.is_some() || at.is_some() {
        let init = resolve_segment_template_init(rt, at, base_url, template)?;

        let media = rt
            .and_then(|t| t.media.clone())
            .or(at.and_then(|t| t.media.clone()));
        let timescale = rt
            .and_then(|t| t.timescale)
            .or(at.and_then(|t| t.timescale))
            .unwrap_or(1);
        let start_number = rt
            .and_then(|t| t.startNumber)
            .or(at.and_then(|t| t.startNumber))
            .unwrap_or(1);

        let segment_timeline = rt
            .and_then(|t| t.SegmentTimeline.as_ref())
            .or(at.and_then(|t| t.SegmentTimeline.as_ref()));

        if let Some(segment_timeline) = segment_timeline {
            debug!("Using (3) SegmentTemplate + SegmentTimeline addressing mode.");

            let Some(media) = media.as_ref() else {
                return Err(Error::DashParse(
                    "Missing @media attribute on SegmentTimeline.".into(),
                ));
            };
            let mut segments = resolve_segment_timeline(
                segment_timeline,
                base_url,
                template,
                period_duration_secs,
                media,
                start_number,
                timescale,
            )?;

            if let Some(first) = segments.first_mut() {
                first.map = init;
            }

            return Ok(segments);
        }

        if let Some(media) = media.as_ref() {
            debug!("Using (4) SegmentTemplate@duration addressing mode.");

            let Some(duration) = rt.and_then(|t| t.duration).or(at.and_then(|t| t.duration)) else {
                return Err(Error::DashParse(
                    "Missing @duration attribute on SegmentTemplate@duration.".into(),
                ));
            };

            // Validate before calculating the live-window offset; malformed zero
            // durations or timescales would otherwise produce infinities and overflow.
            if timescale == 0 {
                return Err(Error::DashParse(
                    "SegmentTemplate @timescale must be greater than zero.".into(),
                ));
            }
            if !duration.is_finite() || duration <= 0.0 {
                return Err(Error::DashParse(
                    "SegmentTemplate @duration must be finite and greater than zero.".into(),
                ));
            }
            if !dynamic_time_offset.is_finite() || dynamic_time_offset < 0.0 {
                return Err(Error::DashParse(
                    "Dynamic segment time offset must be finite and non-negative.".into(),
                ));
            }

            // For dynamic MPDs, offset start_number past expired segments.
            let segment_duration_secs = duration / timescale as f64;
            let expired_segments = (dynamic_time_offset / segment_duration_secs).floor();
            if !expired_segments.is_finite() || expired_segments > u64::MAX as f64 {
                return Err(Error::DashParse(
                    "Dynamic segment number offset exceeds the supported range.".into(),
                ));
            }
            let start_number = start_number
                .checked_add(expired_segments as u64)
                .ok_or_else(|| {
                    Error::DashParse("Segment number overflows the supported range.".into())
                })?;

            let mut segments = resolve_segment_template_duration(
                base_url,
                template,
                period_duration_secs,
                duration,
                media,
                start_number,
                timescale,
            )?;

            if let Some(first) = segments.first_mut() {
                first.map = init;
            }

            return Ok(segments);
        }

        return Ok(Vec::new());
    }

    if let Some(segment_base) = &representation.SegmentBase {
        debug!("Using (5) Representation > SegmentBase addressing mode.");
        return resolve_segment_base(segment_base, base_url, template, config).await;
    }

    if let Some(segment_base) = &adaptation_set.SegmentBase {
        debug!("Using (6) AdaptationSet > SegmentBase addressing mode.");
        return resolve_segment_base(segment_base, base_url, template, config).await;
    }

    if !representation.BaseURL.is_empty() {
        debug!("Using (7) Plain BaseURL addressing mode.");
        return Ok(vec![Segment {
            duration: period_duration_secs as f32,
            uri: base_url.to_string(),
            ..Default::default()
        }]);
    }

    Ok(Vec::new())
}
