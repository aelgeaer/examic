//! Which encoder a build records with, and decoding a recording back to the
//! 16 kHz audio speech recognition takes.

use std::fs::File;
use std::io::{self, BufReader};

use examic_audio::{EncodeError, PacketEncoder};
use ogg::reading::PacketReader;

/// 32 kbit/s VBR mono: transparent for speech, about 14 MB per hour.
#[cfg(any(target_os = "android", feature = "native"))]
const BITRATE_BPS: i32 = 32_000;

#[cfg(any(target_os = "android", feature = "native"))]
pub fn new_encoder(sample_rate: u32) -> Result<impl PacketEncoder + 'static, EncodeError> {
    examic_audio::OpusEncoder::new(sample_rate, BITRATE_BPS)
}

/// Off Android this crate is built only to run the engine's tests, and those
/// must not need CMake and a C compiler. The stand-in keeps Opus framing (20 ms
/// frames, 6.5 ms lookahead); each packet holds just its frame's sample count,
/// which [`new_decoder`]'s stand-in turns back into that much silence.
#[cfg(not(any(target_os = "android", feature = "native")))]
pub fn new_encoder(sample_rate: u32) -> Result<impl PacketEncoder + 'static, EncodeError> {
    Ok(FramingStub { sample_rate })
}

#[cfg(not(any(target_os = "android", feature = "native")))]
struct FramingStub {
    sample_rate: u32,
}

#[cfg(not(any(target_os = "android", feature = "native")))]
impl PacketEncoder for FramingStub {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn frame_samples(&self) -> usize {
        self.sample_rate as usize / 50
    }

    fn lookahead_samples(&self) -> u32 {
        self.sample_rate * 13 / 2000
    }

    fn encode(&mut self, pcm: &[i16], packet: &mut Vec<u8>) -> Result<(), EncodeError> {
        packet.clear();
        packet.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        Ok(())
    }
}

/// Output rate of [`decode_16k`].
pub const DECODE_RATE: u32 = 16_000;

/// Decodes one Opus packet to 16 kHz mono, appending to `out`.
trait PacketDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> io::Result<()>;
}

#[cfg(any(target_os = "android", feature = "native"))]
fn new_decoder(_input_rate: u32) -> io::Result<Box<dyn PacketDecoder>> {
    struct Libopus {
        decoder: opus::Decoder,
        buffer: Vec<f32>,
    }
    impl PacketDecoder for Libopus {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> io::Result<()> {
            let n = self
                .decoder
                .decode_float(packet, &mut self.buffer, false)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            out.extend_from_slice(&self.buffer[..n]);
            Ok(())
        }
    }
    // libopus decodes any stream straight to 16 kHz itself.
    let decoder = opus::Decoder::new(DECODE_RATE, opus::Channels::Mono)
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Box::new(Libopus {
        decoder,
        // 120 ms, the longest Opus packet.
        buffer: vec![0.0; DECODE_RATE as usize * 120 / 1000],
    }))
}

#[cfg(not(any(target_os = "android", feature = "native")))]
fn new_decoder(input_rate: u32) -> io::Result<Box<dyn PacketDecoder>> {
    struct SilenceStub {
        input_rate: u32,
    }
    impl PacketDecoder for SilenceStub {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> io::Result<()> {
            let samples = packet
                .get(..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short packet"))?;
            let n = samples as u64 * DECODE_RATE as u64 / self.input_rate as u64;
            out.resize(out.len() + n as usize, 0.0);
            Ok(())
        }
    }
    Ok(Box::new(SilenceStub { input_rate }))
}

/// Streams an Ogg Opus recording as 16 kHz mono in `-1.0..1.0`, at most
/// `max_samples` of it (the recording's length: what lies beyond is encoder
/// padding). `sink` gets the audio in pieces of a second or so and returns
/// `false` to stop early.
///
/// A file whose end was torn by a crash decodes up to the damage.
pub fn decode_16k(
    file: File,
    max_samples: u64,
    sink: &mut dyn FnMut(&[f32]) -> io::Result<bool>,
) -> io::Result<()> {
    let mut reader = PacketReader::new(BufReader::new(file));
    let invalid = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_owned());
    let head = reader
        .read_packet()
        .map_err(|e| invalid(&e.to_string()))?
        .ok_or_else(|| invalid("empty file"))?;
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        return Err(invalid("not an Ogg Opus stream"));
    }
    let pre_skip_48k = u16::from_le_bytes([head.data[10], head.data[11]]) as u64;
    let input_rate = u32::from_le_bytes(head.data[12..16].try_into().unwrap()).max(1);
    let mut decoder = new_decoder(input_rate)?;

    let mut skip = (pre_skip_48k * DECODE_RATE as u64 / 48_000) as usize;
    let mut remaining = max_samples;
    let mut pcm = Vec::with_capacity(DECODE_RATE as usize * 2);
    let mut seen_tags = false;
    loop {
        let packet = match reader.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // A torn last page after a crash: keep what decoded so far.
            Err(e) => {
                log::warn!("recording ends early: {e}");
                break;
            }
        };
        if !seen_tags {
            seen_tags = true;
            continue; // OpusTags
        }
        decoder.decode(&packet.data, &mut pcm)?;
        if skip > 0 {
            let n = skip.min(pcm.len());
            pcm.drain(..n);
            skip -= n;
        }
        if pcm.len() >= DECODE_RATE as usize {
            if !emit(&mut pcm, &mut remaining, sink)? {
                return Ok(());
            }
            if remaining == 0 {
                return Ok(());
            }
        }
    }
    emit(&mut pcm, &mut remaining, sink)?;
    Ok(())
}

fn emit(
    pcm: &mut Vec<f32>,
    remaining: &mut u64,
    sink: &mut dyn FnMut(&[f32]) -> io::Result<bool>,
) -> io::Result<bool> {
    let n = (pcm.len() as u64).min(*remaining) as usize;
    *remaining -= n as u64;
    let go_on = n == 0 || sink(&pcm[..n])?;
    pcm.clear();
    Ok(go_on)
}
