use serde_json::Value;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub ratio: f64,
    pub duration: f64,
    pub has_audio: bool,
}

pub fn parse_metadata(bytes: &[u8]) -> Result<Metadata, String> {
    let data: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let stream = data["streams"]
        .as_array()
        .and_then(|s| {
            s.iter()
                .find(|v| v["codec_type"] == "video" || v["width"].as_f64().is_some())
        })
        .ok_or("This file has no video stream")?;
    let width = stream["width"].as_f64().unwrap_or(0.0);
    let height = stream["height"].as_f64().unwrap_or(0.0);
    if width <= 0.0 || height <= 0.0 {
        return Err("Invalid video dimensions".into());
    }
    let mut ratio = width / height;
    if let Some((a, b)) = stream["display_aspect_ratio"]
        .as_str()
        .and_then(|s| s.split_once(':'))
        && let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>())
        && a > 0.0
        && b > 0.0
    {
        ratio = a / b;
    }
    if let Some(sides) = stream["side_data_list"].as_array() {
        for side in sides {
            if side["rotation"].as_f64().unwrap_or(0.0).abs() % 180.0 == 90.0 {
                ratio = 1.0 / ratio;
            }
        }
    }
    let duration = data["format"]["duration"]
        .as_str()
        .or_else(|| stream["duration"].as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.0);
    let has_audio = data["streams"]
        .as_array()
        .is_some_and(|streams| streams.iter().any(|s| s["codec_type"] == "audio"));
    Ok(Metadata {
        ratio,
        duration,
        has_audio,
    })
}

/// Metadata probing is polled, never waited for on the input/rendering path.
pub struct Probe {
    child: Option<Child>,
    pub path: PathBuf,
    started: Instant,
}

impl Probe {
    pub fn start(path: PathBuf) -> io::Result<Self> {
        let child = Command::new("ffprobe").args([
            "-v", "error", "-show_entries",
            "stream=codec_type,width,height,display_aspect_ratio,duration:stream_side_data=rotation:format=duration",
            "-of", "json",
        ]).arg(&path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        Ok(Self {
            child: Some(child),
            path,
            started: Instant::now(),
        })
    }

    pub fn poll(&mut self) -> Option<Result<Metadata, String>> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = self.child.take().unwrap().wait_with_output();
                Some(output.map_err(|e| e.to_string()).and_then(|o| {
                    if status.success() {
                        parse_metadata(&o.stdout)
                    } else {
                        Err("Could not read this video".into())
                    }
                }))
            }
            Ok(None) if self.started.elapsed() < Duration::from_secs(15) => None,
            Ok(None) => {
                self.stop();
                Some(Err("Timed out reading video metadata".into()))
            }
            Err(e) => {
                self.stop();
                Some(Err(e.to_string()))
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Frame {
    pub pixels: Vec<u8>,
    pub position: f64,
}

/// At most three decoded frames wait for presentation. Pipe backpressure stops
/// FFmpeg running away while paused; recycled pixel buffers avoid per-frame heaps.
pub struct Decoder {
    child: Option<Child>,
    frames: Option<Receiver<Frame>>,
    recycle: SyncSender<Vec<u8>>,
    worker: Option<JoinHandle<()>>,
    error_worker: Option<JoinHandle<()>>,
    errors: Arc<Mutex<String>>,
}

impl Decoder {
    pub fn start(path: &Path, size: (u16, u16), fps: u16, start: f64) -> io::Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-threads",
                "2",
                "-ss",
            ])
            .arg(start.to_string())
            .arg("-i")
            .arg(path)
            .args(["-map", "0:v:0", "-an", "-sn", "-filter_threads", "1", "-vf"])
            .arg(format!(
                "fps={fps},scale={}:{}:flags=area,setsar=1",
                size.0, size.1
            ))
            .args([
                "-pix_fmt", "rgb24", "-threads", "1", "-f", "rawvideo", "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        let error_copy = errors.clone();
        let error_worker = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.by_ref().take(4096).read_to_end(&mut bytes);
            let _ = io::copy(&mut stderr, &mut io::sink());
            *error_copy.lock().unwrap() = String::from_utf8_lossy(&bytes).trim().to_owned();
        });
        let (send, frames) = mpsc::sync_channel(3);
        let (recycle, buffers) = mpsc::sync_channel(6);
        let worker = thread::spawn(move || {
            let bytes = usize::from(size.0) * usize::from(size.1) * 3;
            let mut index = 0_u64;
            loop {
                let mut pixels = buffers.try_recv().unwrap_or_else(|_| vec![0; bytes]);
                if stdout.read_exact(&mut pixels).is_err() {
                    break;
                }
                let frame = Frame {
                    pixels,
                    position: start + index as f64 / f64::from(fps),
                };
                if send.send(frame).is_err() {
                    break;
                }
                index += 1;
            }
        });
        Ok(Self {
            child: Some(child),
            frames: Some(frames),
            recycle,
            worker: Some(worker),
            error_worker: Some(error_worker),
            errors,
        })
    }

    pub fn receive(&self) -> Result<Frame, TryRecvError> {
        self.frames.as_ref().unwrap().try_recv()
    }

    pub fn recycle(&self, frame: Frame) {
        let _ = self.recycle.try_send(frame.pixels);
    }

    pub fn stop(&mut self) {
        // Disconnect before joining: the reader may be blocked sending a frame.
        self.frames.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.error_worker.take() {
            let _ = worker.join();
        }
    }

    pub fn error(&self) -> String {
        self.errors.lock().unwrap().clone()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn fit(ratio: f64, columns: u16, rows: u16) -> (u16, u16) {
    let width = columns
        .max(1)
        .min((f64::from(rows.max(1)) * ratio * 2.0).round().max(1.0) as u16);
    let height = rows
        .max(1)
        .min((f64::from(width) / ratio / 2.0).round().max(1.0) as u16);
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_seeks_and_cancels_with_a_full_queue() {
        use crate::test_support::TestVideo;
        if !TestVideo::available() {
            eprintln!("FFmpeg unavailable; skipping integration test");
            return;
        }
        let video = TestVideo::new();
        fn frame(decoder: &Decoder) -> Frame {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match decoder.receive() {
                    Ok(frame) => return frame,
                    Err(TryRecvError::Empty) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    result => panic!("decoder did not produce a frame: {:?}", result.err()),
                }
            }
        }
        let first = {
            let mut decoder = Decoder::start(&video.path, (40, 12), 24, 0.0).unwrap();
            let first = frame(&decoder);
            assert_eq!(first.pixels.len(), 40 * 12 * 3);
            assert_eq!(first.position, 0.0);
            thread::sleep(Duration::from_millis(100)); // fill bounded queue
            let start = Instant::now();
            decoder.stop();
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "cancellation blocked on a full queue"
            );
            first
        };
        let decoder = Decoder::start(&video.path, (40, 12), 24, 1.0).unwrap();
        let sought = frame(&decoder);
        assert_eq!(sought.position, 1.0);
        assert_ne!(first.pixels, sought.pixels);
    }

    #[test]
    fn metadata_handles_rotation_and_unknown_duration() {
        let data = br#"{"streams":[{"width":1920,"height":1080,"display_aspect_ratio":"16:9","side_data_list":[{"rotation":-90}]}],"format":{"duration":"N/A"}}"#;
        let meta = parse_metadata(data).unwrap();
        assert!((meta.ratio - 9.0 / 16.0).abs() < 0.001);
        assert_eq!(meta.duration, 0.0);
        assert!(parse_metadata(br#"{"streams":[]}"#).is_err());
        assert!(!meta.has_audio);
        let audio_first = br#"{"streams":[{"codec_type":"audio"},{"codec_type":"video","width":640,"height":360}],"format":{"duration":"3"}}"#;
        assert!(parse_metadata(audio_first).unwrap().has_audio);
    }

    #[test]
    fn fitted_video_stays_inside_viewport() {
        for ratio in [16.0 / 9.0, 9.0 / 16.0, 1.0] {
            let (w, h) = fit(ratio, 80, 24);
            assert!(w <= 80 && h <= 24 && w > 0 && h > 0);
            assert!((f64::from(w) / f64::from(h) / 2.0 - ratio).abs() < 0.1);
        }
        assert_eq!(fit(1.0, 0, 0), (1, 1));
    }
}
