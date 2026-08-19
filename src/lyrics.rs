//! Parsed, presentation-ready lyrics. Raw NCM responses stay outside the TUI.

use std::time::Duration;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Lyrics {
    pub original: Vec<LyricLine>,
    pub translated: Vec<LyricLine>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LyricLine {
    pub start: Duration,
    pub end: Option<Duration>,
    pub text: String,
    pub words: Vec<LyricWord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LyricWord {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

impl Lyrics {
    pub fn from_sources(lrc: Option<&str>, translated: Option<&str>, yrc: Option<&str>) -> Self {
        let original = yrc
            .map(parse_yrc)
            .filter(|lines| !lines.is_empty())
            .unwrap_or_else(|| lrc.map(parse_lrc).unwrap_or_default());
        Self {
            original: finish_lines(original),
            translated: finish_lines(translated.map(parse_lrc).unwrap_or_default()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.original.is_empty()
    }

    pub fn current_index(&self, position: Duration) -> Option<usize> {
        let point = self.original.partition_point(|line| line.start <= position);
        point.checked_sub(1)
    }

    pub fn translation_at(&self, start: Duration) -> Option<&str> {
        let point = self.translated.partition_point(|line| line.start <= start);
        let candidates = [
            point.checked_sub(1),
            (point < self.translated.len()).then_some(point),
        ];
        candidates
            .into_iter()
            .flatten()
            .map(|index| &self.translated[index])
            .filter(|line| line.start.abs_diff(start) <= Duration::from_millis(80))
            .min_by_key(|line| line.start.abs_diff(start))
            .map(|line| line.text.as_str())
    }
}

pub fn parse_lrc(input: &str) -> Vec<LyricLine> {
    let mut offset_ms = 0_i64;
    for raw in input.lines() {
        let line = raw.trim();
        if let Some(value) = line
            .strip_prefix("[offset:")
            .and_then(|value| value.strip_suffix(']'))
            .and_then(|value| value.trim().parse::<i64>().ok())
        {
            offset_ms = value;
        }
    }

    let mut lines = Vec::new();
    for raw in input.lines() {
        let mut rest = raw.trim();
        let mut timestamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(close) = after_open.find(']') else {
                break;
            };
            let tag = &after_open[..close];
            if let Some(time) = parse_lrc_time(tag) {
                timestamps.push(apply_offset(time, offset_ms));
                rest = &after_open[close + 1..];
            } else {
                break;
            }
        }
        let text = rest.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        lines.extend(timestamps.into_iter().map(|start| LyricLine {
            start,
            end: None,
            text: text.clone(),
            words: Vec::new(),
        }));
    }
    lines
}

pub fn parse_yrc(input: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in input.lines() {
        let raw = raw.trim();
        let Some((line_time, mut rest)) = take_bracket(raw) else {
            continue;
        };
        let Some((start_ms, duration_ms)) = parse_yrc_range(line_time) else {
            continue;
        };
        let mut words = Vec::new();
        let mut plain = String::new();
        while let Some(open) = rest.find('(') {
            plain.push_str(&rest[..open]);
            let Some(close_rel) = rest[open + 1..].find(')') else {
                plain.push_str(&rest[open..]);
                rest = "";
                break;
            };
            let close = open + 1 + close_rel;
            let timing = &rest[open + 1..close];
            let after = &rest[close + 1..];
            let next = after.find('(').unwrap_or(after.len());
            let text = &after[..next];
            plain.push_str(text);
            if let Some((word_start, word_duration)) = parse_yrc_range(timing)
                && !text.is_empty()
            {
                words.push(LyricWord {
                    start: Duration::from_millis(word_start),
                    end: Duration::from_millis(word_start.saturating_add(word_duration)),
                    text: text.to_owned(),
                });
            }
            rest = &after[next..];
        }
        plain.push_str(rest);
        let text = plain.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        lines.push(LyricLine {
            start: Duration::from_millis(start_ms),
            end: Some(Duration::from_millis(start_ms.saturating_add(duration_ms))),
            text,
            words,
        });
    }
    lines
}

fn finish_lines(mut lines: Vec<LyricLine>) -> Vec<LyricLine> {
    lines.sort_by_key(|line| line.start);
    lines.dedup_by(|right, left| right.start == left.start && right.text == left.text);
    for index in 0..lines.len().saturating_sub(1) {
        if lines[index].end.is_none() {
            lines[index].end = Some(lines[index + 1].start);
        }
    }
    lines
}

fn parse_lrc_time(value: &str) -> Option<Duration> {
    let (minutes, seconds) = value.split_once(':')?;
    let minutes = minutes.trim().parse::<u64>().ok()?;
    let seconds = seconds.trim().parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(minutes as f64 * 60.0 + seconds))
}

fn apply_offset(time: Duration, offset_ms: i64) -> Duration {
    if offset_ms >= 0 {
        time.saturating_add(Duration::from_millis(offset_ms as u64))
    } else {
        time.saturating_sub(Duration::from_millis(offset_ms.unsigned_abs()))
    }
}

fn take_bracket(input: &str) -> Option<(&str, &str)> {
    let input = input.strip_prefix('[')?;
    let close = input.find(']')?;
    Some((&input[..close], &input[close + 1..]))
}

fn parse_yrc_range(value: &str) -> Option<(u64, u64)> {
    let mut values = value.split(',');
    let start = values.next()?.trim().parse().ok()?;
    let duration = values.next()?.trim().parse().ok()?;
    Some((start, duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_supports_multiple_tags_offset_and_variable_precision() {
        let lines = finish_lines(parse_lrc(
            "[offset:100]\n[00:01.5][00:03.050]hello\n[00:04]world",
        ));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].start, Duration::from_millis(1_600));
        assert_eq!(lines[1].start, Duration::from_millis(3_150));
        assert_eq!(lines[0].end, Some(lines[1].start));
    }

    #[test]
    fn yrc_preserves_word_timings() {
        let lyrics = Lyrics::from_sources(
            None,
            Some("[00:01.00]你好"),
            Some("[1000,2000](1000,500,0)你(1500,500,0)好"),
        );
        assert_eq!(lyrics.original[0].text, "你好");
        assert_eq!(lyrics.original[0].words.len(), 2);
        assert_eq!(lyrics.translation_at(Duration::from_secs(1)), Some("你好"));
    }

    #[test]
    fn current_line_uses_the_latest_started_line() {
        let lyrics = Lyrics::from_sources(Some("[00:01]a\n[00:02]b"), None, None);
        assert_eq!(lyrics.current_index(Duration::from_millis(500)), None);
        assert_eq!(lyrics.current_index(Duration::from_millis(2_500)), Some(1));
    }
}
