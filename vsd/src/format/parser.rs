use crate::{
    error::{Error, Result},
    playlist::{MediaPlaylist, MediaType},
};

#[derive(Clone, Debug, PartialEq)]
pub enum FormatExpr {
    Fallback(Vec<FormatExpr>),
    Merge(Vec<FormatExpr>),
    Single {
        base: BaseFormat,
        filters: Vec<Filter>,
    },
    Index(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum BaseFormat {
    BestVideo,
    BestAudio,
    WorstVideo,
    WorstAudio,
    Sub,
    All,
    AllVid,
    AllAud,
    AllSub,
    AllUnd,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub field: Field,
    pub op: FilterOp,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Width,
    Height,
    Resolution,
    Language,
    Tbr,
    Abr,
    Vbr,
    Fps,
    AudioChannels,
    Acodec,
    Vcodec,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FilterOp {
    Eq,
    Ne,
    Le,
    Ge,
    Lt,
    Gt,
    Contains,
    StartsWith,
    EndsWith,
}

impl FormatExpr {
    /// Split a string by a delimiter, but only at the top level (not inside brackets).
    fn split_top_level(s: &str, delim: char) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut depth = 0;
        let mut start = 0;

        for (i, c) in s.char_indices() {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                c if c == delim && depth == 0 => {
                    parts.push(&s[start..i]);
                    start = i + 1;
                }
                _ => (),
            }
        }
        parts.push(&s[start..]);
        parts
    }

    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::FormatParse("expression cannot be empty".into()));
        }

        // Validate bracket structure before splitting. An unmatched closing
        // bracket otherwise makes the nesting depth negative and can cause
        // delimiters to be interpreted at the wrong level.
        let mut depth = 0usize;
        for ch in s.chars() {
            match ch {
                '[' => depth += 1,
                ']' if depth == 0 => {
                    return Err(Error::FormatParse("unexpected closing bracket".into()));
                }
                ']' => depth -= 1,
                _ => (),
            }
        }
        if depth != 0 {
            return Err(Error::FormatParse("unclosed bracket in format expression".into()));
        }

        // Split by "/" for fallback chains (respecting brackets).
        let fallback_parts = Self::split_top_level(s, '/');

        if fallback_parts.len() == 1 {
            Self::parse_merge(fallback_parts[0].trim())
        } else {
            let mut exprs = Vec::new();
            for part in &fallback_parts {
                exprs.push(Self::parse_merge(part.trim())?);
            }
            Ok(FormatExpr::Fallback(exprs))
        }
    }

    fn parse_merge(s: &str) -> Result<Self> {
        let parts = Self::split_top_level(s, '+');

        if parts.len() == 1 {
            Self::parse_single(parts[0].trim())
        } else {
            let mut exprs = Vec::new();
            for part in &parts {
                exprs.push(Self::parse_single(part.trim())?);
            }
            Ok(FormatExpr::Merge(exprs))
        }
    }

    fn parse_single(s: &str) -> Result<Self> {
        // Try parsing as an integer index first.
        if let Ok(idx) = s.parse::<usize>() {
            return if idx == 0 {
                Err(Error::FormatParse(
                    "stream index must be >= 1, got 0".into(),
                ))
            } else {
                Ok(FormatExpr::Index(idx - 1))
            };
        }

        // Expand shorthand keywords into composite expressions.
        let (base_str, filters_str) = if let Some(bracket_pos) = s.find('[') {
            (&s[..bracket_pos], &s[bracket_pos..])
        } else {
            (s, "")
        };

        match base_str.trim() {
            "b" | "best" => {
                let filters = Self::parse_filters(filters_str)?;
                Ok(FormatExpr::Merge(vec![
                    FormatExpr::Single {
                        base: BaseFormat::BestVideo,
                        filters: filters.clone(),
                    },
                    FormatExpr::Single {
                        base: BaseFormat::BestAudio,
                        filters,
                    },
                ]))
            }
            "w" | "worst" => {
                let filters = Self::parse_filters(filters_str)?;
                Ok(FormatExpr::Merge(vec![
                    FormatExpr::Single {
                        base: BaseFormat::WorstVideo,
                        filters: filters.clone(),
                    },
                    FormatExpr::Single {
                        base: BaseFormat::WorstAudio,
                        filters,
                    },
                ]))
            }
            _ => {
                let base = Self::parse_base(base_str.trim())?;
                let filters = Self::parse_filters(filters_str)?;
                Ok(FormatExpr::Single { base, filters })
            }
        }
    }

    fn parse_base(s: &str) -> Result<BaseFormat> {
        match s {
            "bv" | "bestvideo" => Ok(BaseFormat::BestVideo),
            "ba" | "bestaudio" => Ok(BaseFormat::BestAudio),
            "s" | "sub" => Ok(BaseFormat::Sub),
            "wv" | "worstvideo" => Ok(BaseFormat::WorstVideo),
            "wa" | "worstaudio" => Ok(BaseFormat::WorstAudio),
            "all" => Ok(BaseFormat::All),
            "allvid" => Ok(BaseFormat::AllVid),
            "allaud" => Ok(BaseFormat::AllAud),
            "allsub" => Ok(BaseFormat::AllSub),
            "allund" => Ok(BaseFormat::AllUnd),
            _ => Err(Error::FormatParse(format!("unknown keyword '{}'", s))),
        }
    }

    fn parse_filters(s: &str) -> Result<Vec<Filter>> {
        if s.is_empty() {
            return Ok(Vec::new());
        }

        let mut filters = Vec::new();
        let mut remaining = s;

        while let Some(start) = remaining.find('[') {
            let end = remaining.find(']').ok_or_else(|| {
                Error::FormatParse(format!("unclosed bracket in '{}'", remaining))
            })?;
            let inner = &remaining[start + 1..end];
            filters.push(Self::parse_one_filter(inner)?);
            remaining = &remaining[end + 1..];
        }

        Ok(filters)
    }

    fn parse_one_filter(s: &str) -> Result<Filter> {
        // Try multi-char operators first, then single-char.
        let operators = [
            ("*=", FilterOp::Contains),
            ("^=", FilterOp::StartsWith),
            ("$=", FilterOp::EndsWith),
            ("<=", FilterOp::Le),
            (">=", FilterOp::Ge),
            ("!=", FilterOp::Ne),
            ("<", FilterOp::Lt),
            (">", FilterOp::Gt),
            ("=", FilterOp::Eq),
        ];

        for (op_str, op) in &operators {
            if let Some(pos) = s.find(op_str) {
                let field_str = s[..pos].trim();
                let value = s[pos + op_str.len()..].trim().to_owned();
                let field = Self::parse_field(field_str)?;
                return Ok(Filter {
                    field,
                    op: op.clone(),
                    value,
                });
            }
        }

        Err(Error::FormatParse(format!("invalid filter '[{}]'", s)))
    }

    fn parse_field(s: &str) -> Result<Field> {
        match s {
            "width" => Ok(Field::Width),
            "height" => Ok(Field::Height),
            "resolution" => Ok(Field::Resolution),
            "language" => Ok(Field::Language),
            "tbr" => Ok(Field::Tbr),
            "abr" => Ok(Field::Abr),
            "vbr" => Ok(Field::Vbr),
            "fps" => Ok(Field::Fps),
            "audio_channels" => Ok(Field::AudioChannels),
            "acodec" => Ok(Field::Acodec),
            "vcodec" => Ok(Field::Vcodec),
            _ => Err(Error::FormatParse(format!("unknown field '{}'", s))),
        }
    }

    /// Evaluate a format expression against a slice of media playlists.
    ///
    /// Returns the selected stream indices (0-based), in order.
    pub fn eval(&self, streams: &[MediaPlaylist]) -> Vec<usize> {
        match self {
            FormatExpr::Fallback(exprs) => {
                // Use strict evaluation: a branch is only accepted if every
                // sub-expression in its merge produced at least one result.
                for e in exprs {
                    if let Some(result) = e.eval_strict(streams) {
                        return result;
                    }
                }
                Vec::new()
            }
            FormatExpr::Merge(exprs) => {
                // Lenient: union all sub-expression results, silently
                // skipping any that match nothing.
                let mut merged = Vec::new();
                for e in exprs {
                    for idx in e.eval(streams) {
                        if !merged.contains(&idx) {
                            merged.push(idx);
                        }
                    }
                }
                merged
            }
            FormatExpr::Single { base, filters } => Self::eval_single(streams, base, filters),
            FormatExpr::Index(i) => {
                if *i < streams.len() {
                    vec![*i]
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn eval_strict(&self, streams: &[MediaPlaylist]) -> Option<Vec<usize>> {
        match self {
            FormatExpr::Fallback(exprs) => {
                for e in exprs {
                    if let Some(result) = e.eval_strict(streams) {
                        return Some(result);
                    }
                }
                None
            }
            FormatExpr::Merge(exprs) => {
                let mut merged = Vec::new();
                for e in exprs {
                    let result = e.eval_strict(streams)?;
                    for idx in result {
                        if !merged.contains(&idx) {
                            merged.push(idx);
                        }
                    }
                }
                Some(merged)
            }
            FormatExpr::Single { base, filters } => {
                let result = Self::eval_single(streams, base, filters);
                if result.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }
            FormatExpr::Index(i) => {
                if *i < streams.len() {
                    Some(vec![*i])
                } else {
                    None
                }
            }
        }
    }

    fn eval_single(streams: &[MediaPlaylist], base: &BaseFormat, filters: &[Filter]) -> Vec<usize> {
        let candidates = streams
            .iter()
            .enumerate()
            .filter(|(_, s)| match base {
                BaseFormat::All => true,
                BaseFormat::AllVid | BaseFormat::BestVideo | BaseFormat::WorstVideo => {
                    s.media_type == MediaType::Video
                }
                BaseFormat::AllAud | BaseFormat::BestAudio | BaseFormat::WorstAudio => {
                    s.media_type == MediaType::Audio
                }
                BaseFormat::AllSub | BaseFormat::Sub => s.media_type == MediaType::Subtitles,
                BaseFormat::AllUnd => s.media_type == MediaType::Undefined,
            })
            .filter(|(_, s)| filters.iter().all(|f| Self::matches_filter(s, f)))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();

        match base {
            BaseFormat::BestVideo | BaseFormat::BestAudio | BaseFormat::Sub => {
                candidates.into_iter().take(1).collect()
            }
            BaseFormat::WorstVideo | BaseFormat::WorstAudio => {
                candidates.into_iter().last().into_iter().collect()
            }
            BaseFormat::All
            | BaseFormat::AllVid
            | BaseFormat::AllAud
            | BaseFormat::AllSub
            | BaseFormat::AllUnd => candidates,
        }
    }

    fn matches_filter(stream: &MediaPlaylist, filter: &Filter) -> bool {
        let compare_numeric = |actual: f64, op: &FilterOp, value: &str| {
            const EPS: f64 = 0.001;

            match op {
                FilterOp::Eq => value
                    .split(',')
                    .filter_map(|v| v.trim().parse::<f64>().ok())
                    .any(|v| (actual - v).abs() < EPS),
                FilterOp::Ne => value
                    .split(',')
                    .filter_map(|v| v.trim().parse::<f64>().ok())
                    .all(|v| (actual - v).abs() >= EPS),
                _ => {
                    let Ok(value) = value.trim().parse::<f64>() else {
                        return false;
                    };
                    match op {
                        FilterOp::Le => actual <= value + EPS,
                        FilterOp::Ge => actual >= value - EPS,
                        FilterOp::Lt => actual < value - EPS,
                        FilterOp::Gt => actual > value + EPS,
                        _ => false,
                    }
                }
            }
        };
        let compare_string = |actual: &str, op: &FilterOp, value: &str| {
            let actual = actual.trim().to_lowercase();

            match op {
                FilterOp::Eq => value.split(',').any(|v| {
                    let v = v.trim().to_lowercase();
                    actual == v || actual.starts_with(&format!("{}-", v))
                }),
                FilterOp::Ne => value.split(',').all(|v| {
                    let v = v.trim().to_lowercase();
                    actual != v && !actual.starts_with(&format!("{}-", v))
                }),
                FilterOp::Contains => actual.contains(&value.trim().to_lowercase()),
                FilterOp::StartsWith => actual.starts_with(&value.trim().to_lowercase()),
                FilterOp::EndsWith => actual.ends_with(&value.trim().to_lowercase()),
                _ => false,
            }
        };

        match &filter.field {
            Field::Width => compare_numeric(
                stream.resolution.map(|(w, _)| w as f64).unwrap_or(0.0),
                &filter.op,
                &filter.value,
            ),
            Field::Height => compare_numeric(
                stream.resolution.map(|(_, h)| h as f64).unwrap_or(0.0),
                &filter.op,
                &filter.value,
            ),
            Field::Resolution => compare_string(
                stream
                    .resolution
                    .map(|(w, h)| format!("{}x{}", w, h))
                    .as_deref()
                    .unwrap_or(""),
                &filter.op,
                &filter.value,
            ),
            Field::Language => compare_string(
                stream.language.as_deref().unwrap_or(""),
                &filter.op,
                &filter.value,
            ),
            Field::Tbr | Field::Abr | Field::Vbr => compare_numeric(
                stream.bandwidth.map(|b| b as f64 / 1000.0).unwrap_or(0.0),
                &filter.op,
                &filter.value,
            ),
            Field::Fps => compare_numeric(
                stream.frame_rate.map(|f| f as f64).unwrap_or(0.0),
                &filter.op,
                &filter.value,
            ),
            Field::AudioChannels => compare_numeric(
                stream.channels.map(|c| c as f64).unwrap_or(0.0),
                &filter.op,
                &filter.value,
            ),
            Field::Acodec | Field::Vcodec => compare_string(
                stream.codecs.as_deref().unwrap_or_default(),
                &filter.op,
                &filter.value,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keyword() {
        assert_eq!(
            FormatExpr::parse("bv").unwrap(),
            FormatExpr::Single {
                base: BaseFormat::BestVideo,
                filters: vec![],
            }
        );
    }

    #[test]
    fn parse_index() {
        assert_eq!(FormatExpr::parse("3").unwrap(), FormatExpr::Index(2));
    }

    #[test]
    fn parse_merge() {
        assert_eq!(
            FormatExpr::parse("bv+ba+s").unwrap(),
            FormatExpr::Merge(vec![
                FormatExpr::Single {
                    base: BaseFormat::BestVideo,
                    filters: vec![],
                },
                FormatExpr::Single {
                    base: BaseFormat::BestAudio,
                    filters: vec![],
                },
                FormatExpr::Single {
                    base: BaseFormat::Sub,
                    filters: vec![],
                },
            ])
        );
    }

    #[test]
    fn parse_fallback() {
        if let FormatExpr::Fallback(parts) =
            FormatExpr::parse("bv[height=1080]+ba / bv[height=720]+ba").unwrap()
        {
            assert_eq!(parts.len(), 2);
        } else {
            panic!("expected fallback");
        }
    }

    #[test]
    fn rejects_unbalanced_filter_brackets() {
        assert!(FormatExpr::parse("bv[height=720").is_err());
        assert!(FormatExpr::parse("bv] / ba").is_err());
        assert!(FormatExpr::parse("bv[height=720]]").is_err());
    }

    #[test]
    fn parse_filter() {
        assert_eq!(
            FormatExpr::parse("ba[language=en]").unwrap(),
            FormatExpr::Single {
                base: BaseFormat::BestAudio,
                filters: vec![Filter {
                    field: Field::Language,
                    op: FilterOp::Eq,
                    value: "en".to_owned(),
                }],
            }
        );
    }

    #[test]
    fn parse_filters() {
        if let FormatExpr::Single { filters, .. } =
            FormatExpr::parse("bv[height<=720][vcodec^=avc1]").unwrap()
        {
            assert_eq!(filters.len(), 2);
        } else {
            panic!("expected single");
        }
    }

    fn vid(width: u64, height: u64, bw: u64) -> MediaPlaylist {
        MediaPlaylist {
            media_type: MediaType::Video,
            resolution: Some((width, height)),
            bandwidth: Some(bw),
            codecs: Some("avc1.640028".to_owned()),
            frame_rate: Some(30.0),
            ..Default::default()
        }
    }

    fn aud(lang: &str, bw: u64) -> MediaPlaylist {
        MediaPlaylist {
            media_type: MediaType::Audio,
            language: Some(lang.to_owned()),
            bandwidth: Some(bw),
            codecs: Some("mp4a.40.2".to_owned()),
            channels: Some(2.0),
            ..Default::default()
        }
    }

    fn sub(lang: &str) -> MediaPlaylist {
        MediaPlaylist {
            media_type: MediaType::Subtitles,
            language: Some(lang.to_owned()),
            ..Default::default()
        }
    }

    fn und() -> MediaPlaylist {
        MediaPlaylist {
            media_type: MediaType::Undefined,
            ..Default::default()
        }
    }

    #[test]
    fn eval_best_video() {
        assert_eq!(
            FormatExpr::parse("bv")
                .unwrap()
                .eval(&[vid(1920, 1080, 8000000), vid(1280, 720, 4500000)]),
            [0]
        );
    }

    #[test]
    fn eval_worst_video() {
        assert_eq!(
            FormatExpr::parse("wv")
                .unwrap()
                .eval(&[vid(1920, 1080, 8000000), vid(1280, 720, 4500000)]),
            [1]
        );
    }

    #[test]
    fn eval_merge() {
        assert_eq!(
            FormatExpr::parse("bv+ba+s").unwrap().eval(&[
                vid(1920, 1080, 8000000),
                aud("en", 512000),
                sub("en"),
            ]),
            [0, 1, 2]
        );
    }

    #[test]
    fn eval_index() {
        assert_eq!(
            FormatExpr::parse("1+3").unwrap().eval(&[
                vid(1920, 1080, 8000000),
                aud("en", 512000),
                sub("en"),
            ]),
            [0, 2]
        );
    }

    #[test]
    fn eval_filter_height() {
        assert_eq!(
            FormatExpr::parse("bv[height<=720]").unwrap().eval(&[
                vid(1920, 1080, 8000000),
                vid(1280, 720, 4500000),
                vid(640, 360, 1200000),
            ]),
            [1]
        );
    }

    #[test]
    fn eval_filter_language() {
        assert_eq!(
            FormatExpr::parse("ba[language=fr]").unwrap().eval(&[
                aud("en", 512000),
                aud("fr", 256000),
                aud("es", 128000)
            ]),
            [1]
        );
    }

    #[test]
    fn eval_filter_languages() {
        assert_eq!(
            FormatExpr::parse("allaud[language=en,fr]").unwrap().eval(&[
                aud("en", 512000),
                aud("fr", 256000),
                aud("es", 128000),
            ]),
            [0, 1]
        );
    }

    #[test]
    fn eval_all() {
        assert_eq!(
            FormatExpr::parse("all").unwrap().eval(&[
                vid(1920, 1080, 8000000),
                aud("en", 512000),
                sub("en")
            ]),
            [0, 1, 2]
        );
    }

    #[test]
    fn eval_allvid() {
        assert_eq!(
            FormatExpr::parse("allvid").unwrap().eval(&[
                vid(1920, 1080, 8000000),
                vid(1280, 720, 4500000),
                aud("en", 512000),
            ]),
            [0, 1]
        );
    }

    #[test]
    fn eval_fallback_first_success() {
        assert_eq!(
            FormatExpr::parse("bv[height=1080]+ba / bv[height=720]+ba")
                .unwrap()
                .eval(&[
                    vid(1920, 1080, 8000000),
                    vid(1280, 720, 4500000),
                    aud("en", 512000),
                ]),
            [0, 2]
        );
    }

    #[test]
    fn eval_fallback_first_fail() {
        assert_eq!(
            FormatExpr::parse("bv[height=1080]+ba / bv[height=720]+ba")
                .unwrap()
                .eval(&[vid(1280, 720, 4500000), aud("en", 512000)]),
            [0, 1]
        );
    }

    #[test]
    fn eval_default() {
        assert_eq!(
            FormatExpr::parse("b+s+allund").unwrap().eval(&[
                vid(1920, 1080, 8000000),
                vid(1280, 720, 4500000),
                aud("en", 512000),
                aud("fr", 256000),
                sub("en"),
                sub("fr"),
                und(),
            ]),
            [0, 2, 4, 6]
        );
    }

    #[test]
    fn eval_missing() {
        assert_eq!(
            FormatExpr::parse("b+s+allund")
                .unwrap()
                .eval(&[vid(1920, 1080, 8000000), sub("en")]),
            [0, 1]
        );
    }
}
