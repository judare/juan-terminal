//! WhatsApp voice notes: Ogg/Opus ↔ PCM (libopus), playback with `AVAudioPlayer` and
//! recording with `AVAudioRecorder` (16-bit PCM, then encoded to Ogg/Opus to send).
//! macOS can't play Ogg/Opus itself, so received notes are decoded to a cached WAV first.
#![allow(unexpected_cfgs)] // objc 0.2 macros

use std::io::Cursor;

// Links libopus statically (feature `static`), so the app needs no dylib.
use audiopus_sys as _;
use std::path::{Path, PathBuf};

use objc::runtime::{NO, Object, YES};
use objc::{class, msg_send, sel, sel_impl};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

type Id = *mut Object;

const RATE: u32 = 48_000;

fn ns_string(s: &str) -> Id {
    let c = std::ffi::CString::new(s).unwrap_or_default();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
}

fn file_url(path: &Path) -> Id {
    unsafe { msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.to_string_lossy())] }
}

// ---------------------------------------------------------------- codec

/// Decode an Ogg/Opus file to mono/stereo 16-bit PCM at 48 kHz: (samples interleaved, channels).
pub fn decode_ogg_opus(bytes: &[u8]) -> Result<(Vec<i16>, usize), String> {
    let mut reader = ogg::PacketReader::new(Cursor::new(bytes));
    let head = reader.read_packet().map_err(|e| e.to_string())?.ok_or("archivo vacío")?;
    if !head.data.starts_with(b"OpusHead") || head.data.len() < 12 {
        return Err("no es Opus".into());
    }
    let channels = head.data[9].clamp(1, 2) as usize;
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
    let mut decoder = opus::Decoder::new(RATE, if channels == 2 { opus::Channels::Stereo } else { opus::Channels::Mono }).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut buf = vec![0i16; 5760 * channels];
    while let Some(p) = reader.read_packet().map_err(|e| e.to_string())? {
        if p.data.starts_with(b"OpusTags") {
            continue;
        }
        match decoder.decode(&p.data, &mut buf, false) {
            Ok(n) => out.extend_from_slice(&buf[..n * channels]),
            Err(_) => continue,
        }
    }
    let skip = (pre_skip * channels).min(out.len());
    out.drain(..skip);
    Ok((out, channels))
}

fn wav_bytes(pcm: &[i16], channels: usize, rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&(channels as u16).to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    v.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// A playable file for `media`: Ogg/Opus is decoded to `<media>.wav` once; other formats
/// (mp3, m4a…) play as they are.
pub fn playable(media: &Path) -> Result<PathBuf, String> {
    let is_ogg = matches!(media.extension().and_then(|e| e.to_str()), Some("ogg" | "opus" | "oga"));
    if !is_ogg {
        return Ok(media.to_path_buf());
    }
    let wav = media.with_extension("wav");
    if wav.exists() {
        return Ok(wav);
    }
    let bytes = std::fs::read(media).map_err(|e| e.to_string())?;
    let (pcm, ch) = decode_ogg_opus(&bytes)?;
    std::fs::write(&wav, wav_bytes(&pcm, ch, RATE)).map_err(|e| e.to_string())?;
    Ok(wav)
}

/// Read 16-bit PCM from a WAV file: (samples, channels, rate).
fn read_wav(bytes: &[u8]) -> Option<(Vec<i16>, usize, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let (mut pos, mut channels, mut rate, mut bits) = (12usize, 1usize, RATE, 16u16);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]) as usize;
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            if bits != 16 {
                return None;
            }
            let end = (body + len).min(bytes.len());
            let pcm = bytes[body..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            return Some((pcm, channels.max(1), rate));
        }
        pos = body + len + (len & 1);
    }
    None
}

/// Encode mono 48 kHz PCM to an Ogg/Opus voice note. Returns (ogg bytes, seconds, waveform).
pub fn encode_voice(pcm: &[i16]) -> Result<(Vec<u8>, u32, Vec<u8>), String> {
    const FRAME: usize = 960; // 20 ms
    const PRE_SKIP: u16 = 312;
    let mut enc = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip).map_err(|e| e.to_string())?;
    let _ = enc.set_bitrate(opus::Bitrate::Bits(32_000));
    let mut out = Vec::new();
    {
        let mut w = ogg::PacketWriter::new(&mut out);
        let serial = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1);
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(1);
        head.extend_from_slice(&PRE_SKIP.to_le_bytes());
        head.extend_from_slice(&RATE.to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes());
        head.push(0);
        w.write_packet(head, serial, ogg::PacketWriteEndInfo::EndPage, 0).map_err(|e| e.to_string())?;
        let mut tags = b"OpusTags".to_vec();
        let vendor = b"juan-terminal";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        w.write_packet(tags, serial, ogg::PacketWriteEndInfo::EndPage, 0).map_err(|e| e.to_string())?;
        let frames: Vec<&[i16]> = pcm.chunks(FRAME).collect();
        let mut granule = PRE_SKIP as u64;
        let mut buf = vec![0u8; 4000];
        for (i, f) in frames.iter().enumerate() {
            let mut frame = [0i16; FRAME];
            frame[..f.len()].copy_from_slice(f);
            let n = enc.encode(&frame, &mut buf).map_err(|e| e.to_string())?;
            granule += f.len() as u64;
            let last = i + 1 == frames.len();
            let info = if last {
                ogg::PacketWriteEndInfo::EndStream
            } else if (i + 1) % 50 == 0 {
                ogg::PacketWriteEndInfo::EndPage
            } else {
                ogg::PacketWriteEndInfo::NormalPacket
            };
            w.write_packet(buf[..n].to_vec(), serial, info, granule).map_err(|e| e.to_string())?;
        }
    }
    let seconds = (pcm.len() as f64 / RATE as f64).round().max(1.) as u32;
    Ok((out, seconds, waveform(pcm, 64)))
}

/// `n` amplitude buckets scaled 0–100 (WhatsApp's voice-note waveform).
pub fn waveform(pcm: &[i16], n: usize) -> Vec<u8> {
    if pcm.is_empty() {
        return vec![0; n];
    }
    let chunk = pcm.len().div_ceil(n).max(1);
    let peaks: Vec<f32> = pcm.chunks(chunk).map(|c| c.iter().map(|s| (*s as f32).abs()).sum::<f32>() / c.len() as f32).collect();
    let max = peaks.iter().cloned().fold(1.0f32, f32::max);
    let mut v: Vec<u8> = peaks.iter().map(|p| ((p / max) * 100.).round() as u8).collect();
    v.resize(n, 0);
    v
}

/// Waveform of a received note (for drawing), from its decoded WAV.
pub fn waveform_of(wav: &Path, n: usize) -> Vec<u8> {
    std::fs::read(wav).ok().and_then(|b| read_wav(&b)).map(|(pcm, ch, _)| {
        let mono: Vec<i16> = if ch == 1 { pcm } else { pcm.chunks(ch).map(|c| c[0]).collect() };
        waveform(&mono, n)
    }).unwrap_or_default()
}

// ---------------------------------------------------------------- playback

/// One `AVAudioPlayer` (main thread only).
pub struct Player {
    obj: Id,
}

impl Player {
    pub fn open(path: &Path) -> Option<Self> {
        unsafe {
            let p: Id = msg_send![class!(AVAudioPlayer), alloc];
            let p: Id = msg_send![p, initWithContentsOfURL: file_url(path) error: std::ptr::null_mut::<Id>()];
            if p.is_null() {
                return None;
            }
            let _: bool = msg_send![p, prepareToPlay];
            Some(Self { obj: p })
        }
    }
    pub fn play(&self) {
        unsafe {
            let _: bool = msg_send![self.obj, play];
        }
    }
    pub fn pause(&self) {
        unsafe {
            let _: () = msg_send![self.obj, pause];
        }
    }
    pub fn is_playing(&self) -> bool {
        unsafe {
            let b: objc::runtime::BOOL = msg_send![self.obj, isPlaying];
            b != NO
        }
    }
    pub fn current(&self) -> f64 {
        unsafe { msg_send![self.obj, currentTime] }
    }
    pub fn duration(&self) -> f64 {
        unsafe { msg_send![self.obj, duration] }
    }
    pub fn set_rate(&self, rate: f32) {
        unsafe {
            let _: () = msg_send![self.obj, setEnableRate: YES];
            let _: () = msg_send![self.obj, setRate: rate];
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.obj, stop];
            let _: () = msg_send![self.obj, release];
        }
    }
}

// ---------------------------------------------------------------- recording

pub struct Recorder {
    obj: Id,
    path: PathBuf,
    started: std::time::Instant,
}

impl Recorder {
    /// Start recording mono 48 kHz PCM into a temp WAV. macOS asks for the microphone
    /// permission the first time.
    pub fn start(dir: &Path) -> Result<Self, String> {
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(format!("rec-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&path);
        unsafe {
            let keys = [ns_string("AVFormatIDKey"), ns_string("AVSampleRateKey"), ns_string("AVNumberOfChannelsKey"), ns_string("AVLinearPCMBitDepthKey"), ns_string("AVLinearPCMIsFloatKey"), ns_string("AVLinearPCMIsBigEndianKey")];
            let lpcm: u32 = u32::from_be_bytes(*b"lpcm");
            let vals: [Id; 6] = [
                msg_send![class!(NSNumber), numberWithUnsignedInt: lpcm],
                msg_send![class!(NSNumber), numberWithDouble: RATE as f64],
                msg_send![class!(NSNumber), numberWithInt: 1i32],
                msg_send![class!(NSNumber), numberWithInt: 16i32],
                msg_send![class!(NSNumber), numberWithBool: NO],
                msg_send![class!(NSNumber), numberWithBool: NO],
            ];
            let dict: Id = msg_send![class!(NSDictionary), dictionaryWithObjects: vals.as_ptr() forKeys: keys.as_ptr() count: 6usize];
            let r: Id = msg_send![class!(AVAudioRecorder), alloc];
            let r: Id = msg_send![r, initWithURL: file_url(&path) settings: dict error: std::ptr::null_mut::<Id>()];
            if r.is_null() {
                return Err("No se pudo abrir el micrófono.".into());
            }
            let _: () = msg_send![r, setMeteringEnabled: YES];
            let ok: objc::runtime::BOOL = msg_send![r, record];
            if ok == NO {
                let _: () = msg_send![r, release];
                return Err("No hay permiso de micrófono (Configuración del Sistema → Privacidad → Micrófono).".into());
            }
            Ok(Self { obj: r, path, started: std::time::Instant::now() })
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Current input level 0–1 (for the live indicator).
    pub fn level(&self) -> f32 {
        unsafe {
            let _: () = msg_send![self.obj, updateMeters];
            let db: f32 = msg_send![self.obj, averagePowerForChannel: 0usize];
            ((db + 50.) / 50.).clamp(0., 1.)
        }
    }

    /// Stop and encode: (ogg file, seconds, waveform). `None` if cancelled/too short.
    pub fn finish(self, out_dir: &Path) -> Result<(PathBuf, u32, Vec<u8>), String> {
        unsafe {
            let _: () = msg_send![self.obj, stop];
        }
        let bytes = std::fs::read(&self.path).map_err(|e| e.to_string())?;
        let (pcm, ch, rate) = read_wav(&bytes).ok_or("grabación inválida")?;
        if rate != RATE {
            return Err(format!("frecuencia inesperada ({rate} Hz)"));
        }
        let mono: Vec<i16> = if ch == 1 { pcm } else { pcm.chunks(ch).map(|c| c[0]).collect() };
        if mono.len() < RATE as usize / 2 {
            return Err("La nota de voz es demasiado corta.".into());
        }
        let (ogg, secs, wave) = encode_voice(&mono)?;
        let _ = std::fs::create_dir_all(out_dir);
        let out = out_dir.join(format!("voz-{}.ogg", whatsapp::store::now()));
        std::fs::write(&out, ogg).map_err(|e| e.to_string())?;
        Ok((out, secs, wave))
    }

    pub fn cancel(self) {}
}

impl Drop for Recorder {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.obj, stop];
            let _: () = msg_send![self.obj, release];
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_roundtrip() {
        // 2 s of a 440 Hz tone.
        let pcm: Vec<i16> = (0..RATE as usize * 2).map(|i| ((i as f32 * 440. * std::f32::consts::TAU / RATE as f32).sin() * 12000.) as i16).collect();
        let (ogg, secs, wave) = encode_voice(&pcm).unwrap();
        assert_eq!(secs, 2);
        assert_eq!(wave.len(), 64);
        assert!(wave.iter().all(|w| *w > 80), "steady tone → flat waveform");
        assert!(ogg.starts_with(b"OggS"));
        let (back, ch) = decode_ogg_opus(&ogg).unwrap();
        assert_eq!(ch, 1);
        let diff = (back.len() as i64 - pcm.len() as i64).abs();
        assert!(diff < 960, "length within one frame: {} vs {}", back.len(), pcm.len());
        let energy = |s: &[i16]| s.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / s.len() as f64;
        let ratio = energy(&back[4800..]) / energy(&pcm[4800..]);
        assert!((0.5..1.5).contains(&ratio), "energy ratio {ratio}");
        let wav = wav_bytes(&back, ch, RATE);
        let (pcm2, ch2, rate2) = read_wav(&wav).unwrap();
        assert_eq!((pcm2.len(), ch2, rate2), (back.len(), 1, RATE));
    }
}
