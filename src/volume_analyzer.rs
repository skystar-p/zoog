use crate::{CommentHeader, Error, OpusHeader};
use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use bs1770::{ChannelLoudnessMeter, Power, Windows100ms};
use ogg::Packet;
use std::convert::{TryFrom, TryInto};

// Opus uses this internally so we decode to this regardless of the input file sampling rate
const OPUS_DECODE_SAMPLE_RATE: usize = 48000;

// Specified in RFC6716
const OPUS_MAX_PACKET_DURATION_MS: usize = 120;

#[derive(Clone, Copy, Debug)]
enum State {
    AwaitingHeader,
    AwaitingComments,
    Analyzing,
}

struct DecodeStateChannel {
    loudness_meter: ChannelLoudnessMeter,
    sample_buffer: Vec<f32>,
}

impl DecodeStateChannel {
    fn new(sample_rate: usize) -> DecodeStateChannel {
        DecodeStateChannel {
            loudness_meter: ChannelLoudnessMeter::new(sample_rate as u32),
            sample_buffer: Vec::new(),
        }
    }
}

struct DecodeState {
    channel_count: usize,
    sample_rate: usize,
    decoder: Decoder,
    channel_states: Vec<DecodeStateChannel>,
    sample_buffer: Vec<f32>,
}

impl DecodeState {
    fn new(channel_count: usize, sample_rate: usize) -> Result<DecodeState, Error> {
        let sample_rate_typed = SampleRate::try_from(sample_rate as i32)
            .expect("Unsupported decoding sample rate");
        let channel_count_typed = match channel_count {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => return Err(Error::InvalidChannelCount(n)),
        };
        let decoder = Decoder::new(sample_rate_typed, channel_count_typed)
            .map_err(Error::OpusError)?;
        let mut channel_states = Vec::with_capacity(channel_count);
        for _ in 0..channel_count {
            channel_states.push(DecodeStateChannel::new(sample_rate));
        }
        assert_eq!(channel_states.len(), channel_count);
        let ms_per_second: usize = 1000;
        let state = DecodeState {
            channel_count,
            sample_rate,
            decoder,
            channel_states,
            sample_buffer: vec![0.0f32; channel_count * sample_rate * OPUS_MAX_PACKET_DURATION_MS / ms_per_second],
        };
        Ok(state)
    }

    fn push_packet(&mut self, packet: &[u8]) -> Result<(), Error> {
        // Decode to interleaved PCM
        let decode_fec = false;
        let num_decoded_samples = self.decoder.decode_float(
            Some(packet.try_into().expect("Unable to cast source packet buffer")),
            (&mut self.sample_buffer[..]).try_into().expect("Unable to cast decode buffer"),
            decode_fec
        ).map_err(Error::OpusError)?;

        for (c, channel_state) in &mut self.channel_states.iter_mut().enumerate() {
            channel_state.sample_buffer.resize(num_decoded_samples, 0.0f32);
            // Extract interleaved data
            for i in 0..num_decoded_samples {
                let offset = i * self.channel_count + c;
                channel_state.sample_buffer[i] = self.sample_buffer[offset];
            }
            // Feed to meter
            channel_state.loudness_meter.push(channel_state.sample_buffer.iter().cloned());
        }
        Ok(())
    }

    fn get_windows(&self) -> Windows100ms<Vec<Power>> {
        let windows: Vec<_> = self.channel_states.iter().map(|cs| cs.loudness_meter.as_100ms_windows()).collect();
        // See notes on `reduce_stero` in `bs1770` crate.
        let power_scale_factor = match self.channel_count {
            1 => 2.0, // Since mono is still output to two devices
            2 => 1.0,
            n => panic!("Calculating power for number of channels {} not yet supported", n),
        };
        let num_windows = windows[0].len();
        for channel_windows in &windows {
            assert_eq!(num_windows, channel_windows.len(), "Channels had different amounts of audio");
        }
        let mut result_windows = Vec::with_capacity(num_windows);
        for i in 0..num_windows {
            let mut power = 0.0;
            for channel_windows in &windows {
                let channel_windows = &channel_windows.inner;
                // It would be nice if `Power` implemented addition since this is a
                // semantically-valid operation
                power += channel_windows[i].0;
            }
            power *= power_scale_factor;
            result_windows.push(Power(power));
        }
        Windows100ms{ inner: result_windows }
    }
}

pub struct VolumeAnalyzer {
    decode_state: Option<DecodeState>,
    state: State,
    windows: Windows100ms<Vec<Power>>,
    track_loudness: Vec<f64>,
}

impl VolumeAnalyzer {
    pub fn new() -> VolumeAnalyzer {
        VolumeAnalyzer {
            decode_state: None,
            state: State::AwaitingHeader,
            windows: Windows100ms::new(),
            track_loudness: Vec::new(),
        }
    }

    pub fn submit(&mut self, mut packet: Packet) -> Result<(), Error> {
        match self.state {
            State::AwaitingHeader => {
                let header = OpusHeader::try_new(&mut packet.data)
                        .ok_or(Error::MissingOpusStream)?;
                let channel_count = header.num_output_channels()?;
                let sample_rate = OPUS_DECODE_SAMPLE_RATE;
                self.decode_state = Some(DecodeState::new(channel_count, sample_rate)?);
                self.state = State::AwaitingComments;
            }
            State::AwaitingComments => {
                // Check comment header is valid
                match CommentHeader::try_parse(&mut packet.data) {
                    Ok(Some(_)) => (),
                    Ok(None) => return Err(Error::MissingCommentHeader),
                    Err(e) => return Err(e),
                }
                self.state = State::Analyzing;
            }
            State::Analyzing => {
                let decode_state = self.decode_state.as_mut().expect("Decode state unexpectedly missing");
                decode_state.push_packet(&packet.data)?;
            }
        }
        Ok(())
    }

    fn gated_mean_to_lufs(windows: Windows100ms<&[Power]>) -> f64 {
        let power = bs1770::gated_mean(windows.as_ref());
        if power.0.is_nan() {
            // Near silence can result in a NaN result (https://github.com/ruuda/bs1770/issues/1).
            // Returning a large negative value might result in the application of a massive gain and is therefore
            // not a good idea. Instead we return zero, which indicates the audio is at peak
            // volume.
            0.0
        } else {
            power.loudness_lkfs().into()
        }
    }

    pub fn file_complete(&mut self) {
        if let Some(decode_state) = self.decode_state.take() {
            let windows = decode_state.get_windows();
            let track_power = Self::gated_mean_to_lufs(windows.as_ref());
            self.track_loudness.push(track_power);
            self.windows.inner.extend(windows.inner);
        }
        assert!(self.decode_state.is_none());
        self.state = State::AwaitingHeader;
    }

    pub fn mean_lufs(&self) -> f64 {
        Self::gated_mean_to_lufs(self.windows.as_ref())
    }

    pub fn track_lufs(&self) -> Vec<f64> {
        self.track_loudness.clone()
    }

    pub fn last_track_lufs(&self) -> Option<f64> {
        self.track_loudness.last().cloned()
    }
}
