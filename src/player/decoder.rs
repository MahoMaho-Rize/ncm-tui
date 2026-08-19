use std::{error::Error as StdError, path::Path, time::Duration};

use rodio::{ChannelCount, SampleRate, Source, source::SeekError};
use symphonia::{
    core::{
        audio::{AudioBufferRef, SampleBuffer, SignalSpec},
        codecs::{CODEC_TYPE_NULL, CodecParameters, Decoder, DecoderOptions},
        errors::Error,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo, SeekedTo},
        io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
        probe::Hint,
        units,
    },
    default::{get_codecs, get_probe},
};

#[derive(Clone, Debug, Default)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: Option<u32>,
}

impl AudioInfo {
    pub fn compact(&self) -> String {
        if self.sample_rate == 0 {
            return String::new();
        }
        let rate = if self.sample_rate.is_multiple_of(1_000) {
            format!("{}kHz", self.sample_rate / 1_000)
        } else {
            format!("{:.1}kHz", self.sample_rate as f32 / 1_000.0)
        };
        let depth = self
            .bits_per_sample
            .map(|bits| format!(" · {bits}bit"))
            .unwrap_or_default();
        format!("{} · {rate}{depth}", self.codec.to_uppercase())
    }
}

pub struct PrecisionDecoder {
    decoder: Box<dyn Decoder>,
    format: Box<dyn FormatReader>,
    track_id: u32,
    spec: SignalSpec,
    buffer: SampleBuffer<f32>,
    buffer_offset: usize,
    total_duration: Option<Duration>,
    info: AudioInfo,
}

impl PrecisionDecoder {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let source = Box::new(std::fs::File::open(path)?);
        Self::from_source(source, path.extension().and_then(|value| value.to_str()))
    }

    pub(crate) fn from_source(
        source: Box<dyn MediaSource>,
        extension: Option<&str>,
    ) -> Result<Self, Error> {
        let mut hint = Hint::new();
        if let Some(extension) = extension {
            hint.with_extension(extension);
        }
        let stream = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
        let mut probed = get_probe().format(
            &hint,
            stream,
            &FormatOptions {
                enable_gapless: true,
                prebuild_seek_index: true,
                ..FormatOptions::default()
            },
            &MetadataOptions::default(),
        )?;
        let track = probed
            .format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(Error::Unsupported("no supported audio track"))?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let total_duration = duration_from(&params);
        let info = audio_info(&params);
        let mut decoder = get_codecs().make(&params, &DecoderOptions::default())?;
        let (spec, buffer) = decode_next(&mut *probed.format, &mut *decoder, track_id)?;

        Ok(Self {
            decoder,
            format: probed.format,
            track_id,
            spec,
            buffer,
            buffer_offset: 0,
            total_duration,
            info,
        })
    }

    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    fn refill(&mut self) -> Option<()> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::ResetRequired) => {
                    self.rebuild_decoder().ok()?;
                    continue;
                }
                Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return None;
                }
                Err(_) => return None,
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) if decoded.frames() > 0 => decoded,
                Ok(_) | Err(Error::DecodeError(_)) => continue,
                Err(Error::ResetRequired) => {
                    self.rebuild_decoder().ok()?;
                    continue;
                }
                Err(_) => return None,
            };
            self.spec = *decoded.spec();
            self.buffer = copy_to_f32(decoded, self.spec);
            self.buffer_offset = 0;
            return Some(());
        }
    }

    fn rebuild_decoder(&mut self) -> Result<(), Error> {
        let params = self
            .format
            .tracks()
            .iter()
            .find(|track| track.id == self.track_id)
            .map(|track| track.codec_params.clone())
            .ok_or(Error::Unsupported("audio track disappeared"))?;
        self.decoder = get_codecs().make(&params, &DecoderOptions::default())?;
        Ok(())
    }

    fn seek_precise(&mut self, target: Duration) -> Result<(), DecoderSeekError> {
        let target = self
            .total_duration
            .map(|duration| target.min(duration))
            .unwrap_or(target);
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: target.into(),
                track_id: Some(self.track_id),
            },
        )?;
        self.decoder.reset();
        self.buffer_offset = self.buffer.len();
        self.refine_seek(seeked);
        Ok(())
    }

    fn refine_seek(&mut self, seeked: SeekedTo) {
        let Some(time_base) = self.decoder.codec_params().time_base else {
            return;
        };
        let delta = seeked.required_ts.saturating_sub(seeked.actual_ts);
        let seconds = Duration::from(time_base.calc_time(delta)).as_secs_f64();
        let frames = (seconds * self.spec.rate as f64).ceil() as usize;
        let samples = frames.saturating_mul(self.spec.channels.count());
        for _ in 0..samples {
            if self.next().is_none() {
                break;
            }
        }
    }
}

impl Iterator for PrecisionDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buffer_offset >= self.buffer.len() {
            self.refill()?;
        }
        let sample = *self.buffer.samples().get(self.buffer_offset)?;
        self.buffer_offset += 1;
        Some(sample)
    }
}

impl Source for PrecisionDecoder {
    fn current_span_len(&self) -> Option<usize> {
        match self.buffer.len().saturating_sub(self.buffer_offset) {
            0 => None,
            remaining => Some(remaining),
        }
    }

    fn channels(&self) -> ChannelCount {
        self.spec.channels.count() as ChannelCount
    }

    fn sample_rate(&self) -> SampleRate {
        self.spec.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        self.seek_precise(position)
            .map_err(|error| SeekError::Other(Box::new(error)))
    }
}

#[derive(Debug)]
struct DecoderSeekError(String);

impl std::fmt::Display for DecoderSeekError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for DecoderSeekError {}

impl From<Error> for DecoderSeekError {
    fn from(error: Error) -> Self {
        Self(error.to_string())
    }
}

fn decode_next(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
) -> Result<(SignalSpec, SampleBuffer<f32>), Error> {
    loop {
        let packet = format.next_packet()?;
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) if decoded.frames() > 0 => {
                let spec = *decoded.spec();
                return Ok((spec, copy_to_f32(decoded, spec)));
            }
            Ok(_) | Err(Error::DecodeError(_)) => continue,
            Err(error) => return Err(error),
        }
    }
}

fn copy_to_f32(decoded: AudioBufferRef<'_>, spec: SignalSpec) -> SampleBuffer<f32> {
    let mut buffer = SampleBuffer::new(units::Duration::from(decoded.capacity() as u64), spec);
    buffer.copy_interleaved_ref(decoded);
    buffer
}

fn duration_from(params: &CodecParameters) -> Option<Duration> {
    params
        .time_base
        .zip(params.n_frames)
        .map(|(time_base, frames)| Duration::from(time_base.calc_time(frames)))
}

fn audio_info(params: &CodecParameters) -> AudioInfo {
    let codec = get_codecs()
        .get_codec(params.codec)
        .map(|descriptor| descriptor.short_name.to_owned())
        .unwrap_or_else(|| "audio".to_owned());
    AudioInfo {
        codec,
        sample_rate: params.sample_rate.unwrap_or_default(),
        channels: params
            .channels
            .map(|channels| channels.count() as u16)
            .unwrap_or_default(),
        bits_per_sample: params.bits_per_sample.or(params.bits_per_coded_sample),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn pcm24_wav(samples: &[i32], sample_rate: u32) -> Vec<u8> {
        let data_len = (samples.len() * 3) as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 3).to_le_bytes());
        wav.extend_from_slice(&3_u16.to_le_bytes());
        wav.extend_from_slice(&24_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes()[..3]);
        }
        wav
    }

    #[test]
    fn decodes_24_bit_pcm_at_source_precision_without_audio_device() {
        let input = [-8_388_608, -1, 0, 1, 8_388_607];
        let source = Box::new(Cursor::new(pcm24_wav(&input, 96_000)));
        let mut decoder = PrecisionDecoder::from_source(source, Some("wav")).unwrap();

        assert_eq!(decoder.channels(), 1);
        assert_eq!(decoder.sample_rate(), 96_000);
        assert_eq!(decoder.info().bits_per_sample, Some(24));
        let output: Vec<_> = decoder.by_ref().collect();
        let restored: Vec<_> = output
            .iter()
            .map(|sample| (sample * 8_388_608.0).round() as i32)
            .collect();
        assert_eq!(restored, input);
    }

    #[test]
    fn seeks_to_an_exact_pcm_frame() {
        let input: Vec<_> = (0..1_000).map(|sample| sample - 500).collect();
        let source = Box::new(Cursor::new(pcm24_wav(&input, 1_000)));
        let mut decoder = PrecisionDecoder::from_source(source, Some("wav")).unwrap();

        decoder.try_seek(Duration::from_millis(500)).unwrap();
        let restored = (decoder.next().unwrap() * 8_388_608.0).round() as i32;
        assert_eq!(restored, 0);
    }
}
