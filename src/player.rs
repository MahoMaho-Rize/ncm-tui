//! In-process playback with a project-owned Symphonia pipeline and CPAL output.

mod decoder;

use std::{
    path::Path,
    time::{Duration, Instant},
};

pub use decoder::AudioInfo;
use decoder::PrecisionDecoder;
use rodio::{OutputStream, OutputStreamBuilder, Sink, Source};
use symphonia::core::io::MediaSource;
use thiserror::Error;

#[derive(Clone, Debug, Default)]
pub struct PlayerState {
    pub title: String,
    pub artists: String,
    pub elapsed: Duration,
    pub duration: Duration,
    /// Monotonic, high-precision progress sampled from the playback clock.
    pub progress: f64,
    pub paused: bool,
    pub volume: f32,
    pub finished: bool,
    pub audio: AudioInfo,
}

#[derive(Debug, Error)]
pub enum PlayerError {
    #[error("audio output unavailable: {0}")]
    Output(String),
    #[error("cannot open audio file: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot decode audio: {0}")]
    Decode(String),
    #[error("cannot seek: {0}")]
    Seek(String),
}

pub struct Player {
    stream: OutputStream,
    sink: Sink,
    title: String,
    artists: String,
    duration: Duration,
    volume: f32,
    audio: AudioInfo,
    clock: PlaybackClock,
}

#[derive(Clone, Debug)]
struct PlaybackClock {
    anchor_position: Duration,
    anchor_instant: Instant,
    paused: bool,
    duration: Duration,
    started: bool,
}

impl PlaybackClock {
    fn new() -> Self {
        Self {
            anchor_position: Duration::ZERO,
            anchor_instant: Instant::now(),
            paused: true,
            duration: Duration::ZERO,
            started: false,
        }
    }

    fn reset(&mut self, duration: Duration) {
        self.anchor_position = Duration::ZERO;
        self.anchor_instant = Instant::now();
        self.paused = false;
        self.duration = duration;
        self.started = true;
    }

    fn position(&self) -> Duration {
        self.position_at(Instant::now())
    }

    fn position_at(&self, now: Instant) -> Duration {
        if !self.started {
            return Duration::ZERO;
        }
        let position = if self.paused {
            self.anchor_position
        } else {
            self.anchor_position
                .saturating_add(now.saturating_duration_since(self.anchor_instant))
        };
        if self.duration.is_zero() {
            position
        } else {
            position.min(self.duration)
        }
    }

    fn toggle(&mut self) {
        if !self.started {
            return;
        }
        let now = Instant::now();
        if self.paused {
            self.anchor_instant = now;
            self.paused = false;
        } else {
            self.anchor_position = self.position_at(now);
            self.paused = true;
        }
    }

    fn seek(&mut self, position: Duration) {
        if !self.started {
            return;
        }
        self.anchor_position = if self.duration.is_zero() {
            position
        } else {
            position.min(self.duration)
        };
        self.anchor_instant = Instant::now();
    }
}

impl Player {
    pub fn new() -> Result<Self, PlayerError> {
        let stream = OutputStreamBuilder::open_default_stream()
            .map_err(|error| PlayerError::Output(error.to_string()))?;
        let sink = Sink::connect_new(stream.mixer());
        sink.set_volume(0.72);
        Ok(Self {
            stream,
            sink,
            title: String::new(),
            artists: String::new(),
            duration: Duration::ZERO,
            volume: 0.72,
            audio: AudioInfo::default(),
            clock: PlaybackClock::new(),
        })
    }

    pub fn play(
        &mut self,
        path: &Path,
        title: impl Into<String>,
        artists: impl Into<String>,
    ) -> Result<(), PlayerError> {
        let decoder =
            PrecisionDecoder::open(path).map_err(|error| PlayerError::Decode(error.to_string()))?;
        self.append_decoder(decoder, title, artists);
        Ok(())
    }

    pub fn play_source(
        &mut self,
        source: Box<dyn MediaSource>,
        extension: Option<&str>,
        title: impl Into<String>,
        artists: impl Into<String>,
    ) -> Result<(), PlayerError> {
        let decoder = PrecisionDecoder::from_source(source, extension)
            .map_err(|error| PlayerError::Decode(error.to_string()))?;
        self.append_decoder(decoder, title, artists);
        Ok(())
    }

    fn append_decoder(
        &mut self,
        decoder: PrecisionDecoder,
        title: impl Into<String>,
        artists: impl Into<String>,
    ) {
        self.duration = decoder.total_duration().unwrap_or_default();
        self.audio = decoder.info().clone();
        self.sink.stop();
        self.sink = Sink::connect_new(self.stream.mixer());
        self.sink.set_volume(self.volume);
        self.sink.append(decoder);
        self.title = title.into();
        self.artists = artists.into();
        self.clock.reset(self.duration);
    }

    pub fn toggle_pause(&mut self) {
        if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
        self.clock.toggle();
    }

    pub fn seek_by(&mut self, delta: i64) -> Result<(), PlayerError> {
        let current = self.clock.position().as_secs_f64();
        let target = Duration::from_secs_f64((current + delta as f64).max(0.0));
        let target = target.min(self.duration);
        self.sink
            .try_seek(target.min(self.duration))
            .map_err(|error| PlayerError::Seek(error.to_string()))?;
        self.clock.seek(target);
        Ok(())
    }

    pub fn seek_to(&mut self, position: Duration) -> Result<(), PlayerError> {
        let position = position.min(self.duration);
        self.sink
            .try_seek(position)
            .map_err(|error| PlayerError::Seek(error.to_string()))?;
        self.clock.seek(position);
        Ok(())
    }

    pub fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.sink.set_volume(self.volume);
    }

    pub fn state(&self) -> PlayerState {
        let elapsed = self.clock.position();
        PlayerState {
            title: self.title.clone(),
            artists: self.artists.clone(),
            elapsed,
            duration: self.duration,
            progress: smooth_progress(elapsed, self.duration),
            paused: self.sink.is_paused(),
            volume: self.volume,
            finished: !self.title.is_empty() && self.sink.empty(),
            audio: self.audio.clone(),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.title.is_empty() && !self.sink.is_paused() && !self.sink.empty()
    }

    pub fn is_finished(&self) -> bool {
        !self.title.is_empty() && self.sink.empty()
    }
}

fn smooth_progress(elapsed: Duration, duration: Duration) -> f64 {
    if duration.is_zero() {
        return 0.0;
    }
    (elapsed.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_clock_is_smooth_pauseable_seekable_and_clamped() {
        let start = Instant::now();
        let mut clock = PlaybackClock {
            anchor_position: Duration::from_millis(250),
            anchor_instant: start,
            paused: false,
            duration: Duration::from_secs(2),
            started: true,
        };
        assert_eq!(
            clock.position_at(start + Duration::from_micros(5_555)),
            Duration::from_micros(255_555)
        );
        assert!(
            clock.position_at(start + Duration::from_millis(20))
                >= clock.position_at(start + Duration::from_millis(10))
        );

        clock.anchor_position = clock.position_at(start + Duration::from_millis(50));
        clock.paused = true;
        assert_eq!(
            clock.position_at(start + Duration::from_secs(1)),
            Duration::from_millis(300)
        );

        clock.paused = false;
        clock.anchor_instant = start + Duration::from_secs(1);
        assert_eq!(
            clock.position_at(start + Duration::from_millis(1_100)),
            Duration::from_millis(400)
        );

        clock.anchor_position = Duration::from_millis(1_900);
        clock.anchor_instant = start;
        assert_eq!(
            clock.position_at(start + Duration::from_secs(1)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn idle_clock_stays_at_zero() {
        let clock = PlaybackClock::new();
        let start = clock.anchor_instant;

        assert_eq!(
            clock.position_at(start + Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn progress_function_preserves_180_hz_samples() {
        let duration = Duration::from_secs(10);
        let frame = Duration::from_nanos(5_555_556);
        let first = smooth_progress(frame, duration);
        let second = smooth_progress(frame.saturating_mul(2), duration);

        assert!(first > 0.0);
        assert!(second > first);
        assert!((second - first - frame.as_secs_f64() / 10.0).abs() < f64::EPSILON);
        assert_eq!(smooth_progress(duration, duration), 1.0);
    }
}
