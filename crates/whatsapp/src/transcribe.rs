//! Voice-note transcription with whisper.cpp (`whisper-cli`, Homebrew) and a local ggml model.
//! ffmpeg turns the Ogg/Opus note into the 16 kHz mono WAV whisper expects.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn find_bin(name: &str) -> Option<PathBuf> {
    // Apps launched from Finder don't get the shell's PATH: try the usual places first.
    let fixed = ["/opt/homebrew/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(name));
    let path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).map(|d| d.join(name)).collect::<Vec<_>>());
    fixed.chain(path).find(|p| p.is_file())
}

/// Best model available: `$WHISPER_MODEL`, else the largest multilingual ggml in the usual dirs.
fn find_model() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WHISPER_MODEL").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let dirs = [home.join(".cache/whisper"), home.join(".whisper"), PathBuf::from("/opt/homebrew/share/whisper-cpp/models")];
    // Preference order: good Spanish quality at a few seconds per note.
    let names = ["ggml-small.bin", "ggml-medium.bin", "ggml-large-v3-turbo.bin", "ggml-large-v3.bin", "ggml-base.bin", "ggml-tiny.bin"];
    names.iter().flat_map(|n| dirs.iter().map(move |d| d.join(n))).find(|p| p.is_file())
}

/// Whether whisper-cli, ffmpeg and a model are installed (checked once per run).
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| find_bin("whisper-cli").is_some() && find_bin("ffmpeg").is_some() && find_model().is_some())
}

/// Transcribe an audio file (blocking; a few seconds per minute of audio).
pub fn transcribe(media: &Path, work_dir: &Path) -> Result<String, String> {
    let whisper = find_bin("whisper-cli").ok_or("whisper-cli no está instalado (brew install whisper-cpp)")?;
    let ffmpeg = find_bin("ffmpeg").ok_or("ffmpeg no está instalado")?;
    let model = find_model().ok_or("no hay modelo de whisper en ~/.cache/whisper")?;
    std::fs::create_dir_all(work_dir).map_err(|e| e.to_string())?;
    let wav = work_dir.join(format!("{}-{}.wav", std::process::id(), media.file_stem().and_then(|s| s.to_str()).unwrap_or("audio")));
    let ok = background(&ffmpeg)
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(media)
        .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(&wav)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if !ok {
        let _ = std::fs::remove_file(&wav);
        return Err("ffmpeg no pudo convertir el audio".into());
    }
    let threads = std::thread::available_parallelism().map(|n| n.get().clamp(2, 4)).unwrap_or(4).to_string();
    let out = background(&whisper).arg("-m").arg(&model).args(["-l", "auto", "-nt", "-np", "-t", &threads, "-f"]).arg(&wav).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let _ = std::fs::remove_file(&wav);
    let out = out.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("whisper-cli falló".into());
    }
    Ok(clean(&String::from_utf8_lossy(&out.stdout)))
}

/// `cmd` at background QoS (`taskpolicy -b`): low priority, efficiency cores, so transcribing
/// never makes the app or the rest of the Mac feel slow.
fn background(cmd: &Path) -> Command {
    let mut c = Command::new("/usr/sbin/taskpolicy");
    c.arg("-b").arg(cmd);
    c
}

/// Join whisper's lines and drop its non-speech markers ("[BLANK_AUDIO]", "[Música]"…).
fn clean(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !(l.starts_with('[') && l.ends_with(']')))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleans_markers() {
        assert_eq!(super::clean(" Hola\n[BLANK_AUDIO]\n qué tal \n"), "Hola qué tal");
    }
}
