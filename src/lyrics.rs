//! Parsed, presentation-ready lyrics. Raw NCM responses stay outside the TUI.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use lofty::prelude::{ItemKey, TaggedFileExt};

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

    pub fn from_local_document(original: &str, translated: Option<&str>) -> Self {
        if let Some(translated) = translated.filter(|text| !text.trim().is_empty()) {
            return Self::from_sources(Some(original), Some(translated), None);
        }
        split_bilingual_lrc(original)
    }

    pub async fn load_local(path: &Path) -> Option<Self> {
        let (sidecar, translation) = read_sidecar_pair(path).await;
        if let Some(original) = sidecar.as_deref() {
            let lyrics = Self::from_local_document(original, translation.as_deref());
            if !lyrics.is_empty() {
                return Some(lyrics);
            }
        }
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || read_embedded_lyrics(&path))
            .await
            .ok()
            .flatten()
            .and_then(|text| {
                let lyrics = Self::from_local_document(&text, None);
                (!lyrics.is_empty()).then_some(lyrics)
            })
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

fn split_bilingual_lrc(input: &str) -> Lyrics {
    let mut lines = parse_lrc(input);
    lines.sort_by_key(|line| line.start);
    let mut original = Vec::new();
    let mut translated = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let current = lines[index].clone();
        let paired = lines.get(index + 1).filter(|next| {
            next.start.abs_diff(current.start) <= Duration::from_millis(80)
                && next.text != current.text
        });
        if let Some(next) = paired {
            original.push(current);
            translated.push(next.clone());
            index += 2;
        } else {
            original.push(current);
            index += 1;
        }
    }
    Lyrics {
        original: finish_lines(original),
        translated: finish_lines(translated),
    }
}

async fn read_sidecar_pair(path: &Path) -> (Option<String>, Option<String>) {
    let mut original = None;
    let mut translated = None;
    for candidate in sidecar_candidates(path) {
        let Ok(text) = tokio::fs::read_to_string(&candidate).await else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if is_translation_sidecar(&candidate) {
            translated.get_or_insert(text);
        } else {
            original.get_or_insert(text);
        }
    }
    (original, translated)
}

fn sidecar_candidates(path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![path.with_extension("lrc"), path.with_extension("LRC")];
    let Some(directory) = path.parent() else {
        return paths;
    };
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return paths;
    };
    for suffix in [".tlyric.lrc", ".tlyric", ".trans.lrc", ".zh.lrc", ".cn.lrc"] {
        paths.push(directory.join(format!("{stem}{suffix}")));
    }
    paths
}

fn is_translation_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains(".tlyric")
                || lower.contains(".trans.")
                || lower.ends_with(".zh.lrc")
                || lower.ends_with(".cn.lrc")
        })
}

fn read_embedded_lyrics(path: &Path) -> Option<String> {
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    tag.get_string(ItemKey::UnsyncLyrics)
        .or_else(|| tag.get_string(ItemKey::Lyrics))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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

    #[test]
    fn bilingual_sidecar_pairs_same_timestamp_as_translation() {
        let lyrics = Lyrics::from_local_document(
            "[00:01.00]夜に駆ける\n[00:01.00]奔向夜晚\n[00:05.00]skip\n[00:08.00]朝を待つ\n[00:08.00]等待黎明",
            None,
        );
        assert_eq!(lyrics.original.len(), 3);
        assert_eq!(lyrics.original[0].text, "夜に駆ける");
        assert_eq!(
            lyrics.translation_at(Duration::from_secs(1)),
            Some("奔向夜晚")
        );
        assert_eq!(
            lyrics.translation_at(Duration::from_secs(8)),
            Some("等待黎明")
        );
        assert!(lyrics.translation_at(Duration::from_secs(5)).is_none());
    }

    #[tokio::test]
    async fn load_local_reads_sidecar_before_embedded() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("track.flac");
        let lyrics = directory.path().join("track.lrc");
        std::fs::write(&audio, b"not-a-real-flac").unwrap();
        std::fs::write(&lyrics, "[00:01.00]本地原词\n[00:01.00]本地译文\n").unwrap();

        let loaded = Lyrics::load_local(&audio).await.unwrap();
        assert_eq!(loaded.original[0].text, "本地原词");
        assert_eq!(
            loaded.translation_at(Duration::from_secs(1)),
            Some("本地译文")
        );
    }
}
