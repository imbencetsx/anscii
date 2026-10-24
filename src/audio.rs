//! Bounded PCM decoding and native output. The real-time callback never allocates
//! or locks; playback follows samples delivered to the device, not a wall timer.
use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ringbuf::{
    HeapCons, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::{
    io::{self, Read},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Shared {
    playing: AtomicBool,
    cancelled: AtomicBool,
    ready: AtomicBool,
    ended: AtomicBool,
    eof: AtomicBool,
    failed: AtomicBool,
    rate: AtomicU32,
    played: AtomicU64,
    gain: AtomicU32,
    underruns: AtomicU64,
    reader: OnceLock<Thread>,
    error: Mutex<String>,
}

impl Shared {
    fn fail(&self, error: impl ToString) {
        *self.error.lock().unwrap() = error.to_string();
        self.failed.store(true, Ordering::Release);
    }
    fn wake_reader(&self) {
        if let Some(reader) = self.reader.get() {
            reader.unpark();
        }
    }
}

pub struct Audio {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    start: f64,
    created: Instant,
}

impl Audio {
    pub fn start(path: &Path, start: f64, gain: f32) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.gain.store(gain.to_bits(), Ordering::Relaxed);
        let state = shared.clone();
        let path = path.to_owned();
        let worker = thread::Builder::new()
            .name("anscii-audio".into())
            .spawn(move || {
                if let Err(error) = run(&path, start, state.clone()) {
                    state.fail(error);
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
            start,
            created: Instant::now(),
        })
    }

    pub fn ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }
    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Acquire)
    }
    pub fn playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }
    pub fn play(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Release);
        self.shared.wake_reader();
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub fn gain(&self, gain: f32) {
        self.shared
            .gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn position(&self) -> f64 {
        self.start
            + self.shared.played.load(Ordering::Acquire) as f64
                / f64::from(self.shared.rate.load(Ordering::Relaxed).max(1))
    }
    pub fn error(&self) -> Option<String> {
        if self.shared.failed.load(Ordering::Acquire) {
            Some(self.shared.error.lock().unwrap().clone())
        } else if !self.ready() && self.created.elapsed() > Duration::from_secs(5) {
            Some("Audio output timed out".into())
        } else {
            None
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shared.playing.store(false, Ordering::Release);
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.wake_reader();
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

struct Mixer {
    consumer: HeapCons<f32>,
    shared: Arc<Shared>,
    channels: usize,
    rate: u32,
    consumed: u64,
    silence: u64,
    scratch: [f32; 4096],
}

impl Mixer {
    fn render<T: SizedSample + FromSample<f32>>(&mut self, output: &mut [T], latency: Duration) {
        output.fill(T::EQUILIBRIUM);
        if !self.shared.playing.load(Ordering::Acquire) {
            return;
        }
        let gain = f32::from_bits(self.shared.gain.load(Ordering::Relaxed));
        let chunk_size = self.scratch.len() / self.channels * self.channels;
        for out in output.chunks_mut(chunk_size) {
            let available =
                self.consumer.occupied_len().min(out.len()) / self.channels * self.channels;
            let received = self.consumer.pop_slice(&mut self.scratch[..available]);
            for (out, sample) in out.iter_mut().zip(&self.scratch[..received]) {
                *out = T::from_sample((sample * gain).clamp(-1.0, 1.0));
            }
            let frames = (received / self.channels) as u64;
            let silent = ((out.len() - received) / self.channels) as u64;
            self.consumed += frames;
            if frames > 0 {
                self.silence = silent;
            } else {
                self.silence += silent;
            }
            if silent > 0 && !self.shared.eof.load(Ordering::Acquire) {
                self.shared.underruns.fetch_add(silent, Ordering::Relaxed);
            }
        }
        // Samples queued for a future device callback are not audible yet.
        // Silence after EOF drains that latency so the final samples can finish.
        let queued = (latency.as_secs_f64() * f64::from(self.rate)) as u64
            + (output.len() / self.channels) as u64;
        let played = self
            .consumed
            .saturating_sub(queued.saturating_sub(self.silence));
        self.shared.played.fetch_max(played, Ordering::Release);
        if self.shared.eof.load(Ordering::Acquire)
            && self.consumer.is_empty()
            && played == self.consumed
        {
            self.shared.ended.store(true, Ordering::Release);
        }
        if self.consumer.occupied_len() < self.consumer.capacity().get() / 2 {
            self.shared.wake_reader();
        }
    }
}

fn stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut mixer: Mixer,
) -> Result<cpal::Stream, String> {
    let errors = mixer.shared.clone();
    device
        .build_output_stream(
            config,
            move |output: &mut [T], info: &cpal::OutputCallbackInfo| {
                let timestamp = info.timestamp();
                let latency = timestamp.playback.duration_since(timestamp.callback);
                mixer.render(output, latency);
            },
            move |error| errors.fail(error),
            Some(Duration::from_secs(2)),
        )
        .map_err(|e| e.to_string())
}

fn pcm_command(path: &Path, start: f64, rate: u32, channels: usize) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-ss",
        ])
        .arg(start.to_string())
        .arg("-i")
        .arg(path)
        .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-af"])
        .arg(format!("aresample={rate}:async=1:first_pts=0"))
        .args([
            "-ac",
            &channels.to_string(),
            "-ar",
            &rate.to_string(),
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn run(path: &Path, start: f64, shared: Arc<Shared>) -> Result<(), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("No audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate;
    let channels = usize::from(config.channels);
    if channels == 0 || channels > 128 || rate == 0 {
        return Err("Unsupported audio device configuration".into());
    }
    shared.rate.store(rate, Ordering::Relaxed);
    let capacity = rate as usize * channels / 2; // half a second, regardless of movie length
    let (mut producer, consumer) = HeapRb::<f32>::new(capacity).split();
    let mixer = Mixer {
        consumer,
        shared: shared.clone(),
        channels,
        rate,
        consumed: 0,
        silence: 0,
        scratch: [0.0; 4096],
    };
    let stream = match format {
        cpal::SampleFormat::F32 => stream::<f32>(&device, config, mixer),
        cpal::SampleFormat::F64 => stream::<f64>(&device, config, mixer),
        cpal::SampleFormat::I16 => stream::<i16>(&device, config, mixer),
        cpal::SampleFormat::I32 => stream::<i32>(&device, config, mixer),
        cpal::SampleFormat::U16 => stream::<u16>(&device, config, mixer),
        _ => Err(format!("Unsupported audio sample format: {format}")),
    }?;
    let mut child = pcm_command(path, start, rate, channels)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let reader_state = shared.clone();
    let reader = thread::Builder::new()
        .name("anscii-pcm".into())
        .spawn(move || {
            let _ = reader_state.reader.set(thread::current());
            let mut bytes = [0_u8; 16384];
            let mut samples = [0_f32; 4096];
            let mut filled = 0;
            while !reader_state.cancelled.load(Ordering::Acquire) {
                let read = match stdout.read(&mut bytes[filled..]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                filled += read;
                let count = filled / (channels * 4) * channels;
                for (sample, raw) in samples[..count]
                    .iter_mut()
                    .zip(bytes[..count * 4].as_chunks::<4>().0)
                {
                    *sample = f32::from_le_bytes(*raw);
                }
                let mut offset = 0;
                while offset < count && !reader_state.cancelled.load(Ordering::Acquire) {
                    let writable = producer.vacant_len().min(count - offset) / channels * channels;
                    if writable == 0 {
                        // The consumer wakes us after freeing space; play/stop
                        // also wake us. A paused full ring needs no timer.
                        thread::park();
                        continue;
                    }
                    offset += producer.push_slice(&samples[offset..offset + writable]);
                    if producer.occupied_len() >= rate as usize * channels / 20 {
                        reader_state.ready.store(true, Ordering::Release);
                    }
                }
                filled -= count * 4;
                bytes.copy_within(count * 4..count * 4 + filled, 0);
            }
            reader_state.eof.store(true, Ordering::Release);
            reader_state.ready.store(true, Ordering::Release);
        })
        .map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            e.to_string()
        })?;
    let result = stream.play().map_err(|e| e.to_string());
    if result.is_ok() {
        let mut was_playing = true;
        while !shared.cancelled.load(Ordering::Acquire) && !shared.failed.load(Ordering::Acquire) {
            let playing = shared.playing.load(Ordering::Acquire);
            if playing != was_playing {
                // Stop native callbacks while paused instead of running a silent
                // high-priority audio thread for the whole pause.
                if playing {
                    let _ = stream.play();
                } else {
                    let _ = stream.pause();
                }
                was_playing = playing;
            }
            if let Ok(Some(status)) = child.try_wait()
                && !status.success()
            {
                shared.fail("FFmpeg could not decode this audio track");
                break;
            }
            thread::park_timeout(if playing {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(250)
            });
        }
    }
    shared.cancelled.store(true, Ordering::Release);
    shared.wake_reader();
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    drop(stream);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_pcm_seeking_trims_to_the_requested_media_sample() {
        use crate::test_support::TestVideo;
        if !TestVideo::available() {
            eprintln!("FFmpeg unavailable; skipping integration test");
            return;
        }
        let video = TestVideo::new();
        let ramp = video.folder.join("audio ramp.wav");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.1+0.1*t:s=48000:d=2",
                "-c:a",
                "pcm_f32le",
            ])
            .arg(&ramp)
            .status()
            .unwrap();
        assert!(status.success());
        let decoded = pcm_command(&ramp, 1.0, 48000, 1).output().unwrap();
        assert!(decoded.status.success());
        let first = f32::from_le_bytes(decoded.stdout[..4].try_into().unwrap());
        let last = f32::from_le_bytes(
            decoded.stdout[decoded.stdout.len() - 4..]
                .try_into()
                .unwrap(),
        );
        assert!(
            (first - 0.2).abs() < 0.0001,
            "seek started at incorrect sample: {first}"
        );
        assert!((last - 0.3).abs() < 0.0001);
        assert_eq!(decoded.stdout.len(), 48000 * 4);
    }

    #[test]
    fn callback_pause_volume_and_latency_clock() {
        let shared = Arc::new(Shared::default());
        shared.gain.store(0.5_f32.to_bits(), Ordering::Relaxed);
        let (mut producer, consumer) = HeapRb::new(128).split();
        producer.push_slice(&[0.8; 64]);
        let mut mixer = Mixer {
            consumer,
            shared: shared.clone(),
            channels: 2,
            rate: 1000,
            consumed: 0,
            silence: 0,
            scratch: [0.0; 4096],
        };
        let mut out = [1.0_f32; 16];
        mixer.render(&mut out, Duration::from_millis(4));
        assert_eq!(out, [0.0; 16]);
        assert_eq!(mixer.consumer.occupied_len(), 64);
        shared.playing.store(true, Ordering::Release);
        mixer.render(&mut out, Duration::from_millis(4));
        assert_eq!(out, [0.4; 16]);
        assert_eq!(shared.played.load(Ordering::Relaxed), 0); // still queued at device
        mixer.render(&mut out, Duration::from_millis(4));
        assert_eq!(shared.played.load(Ordering::Relaxed), 4);
        shared.playing.store(false, Ordering::Release);
        let remaining = mixer.consumer.occupied_len();
        mixer.render(&mut out, Duration::ZERO);
        assert_eq!(mixer.consumer.occupied_len(), remaining);
        assert_eq!(shared.played.load(Ordering::Relaxed), 4);
        shared.playing.store(true, Ordering::Release);
        shared.gain.store(0.0_f32.to_bits(), Ordering::Relaxed);
        mixer.render(&mut out, Duration::ZERO);
        assert_eq!(out, [0.0; 16]); // muted, but still consuming media
        assert!(shared.played.load(Ordering::Relaxed) > 4);
    }

    #[test]
    fn eof_drains_the_device_tail_and_underruns_do_not_advance_media() {
        let shared = Arc::new(Shared::default());
        shared.playing.store(true, Ordering::Release);
        let (mut producer, consumer) = HeapRb::new(128).split();
        producer.push_slice(&[0.5; 16]);
        let mut mixer = Mixer {
            consumer,
            shared: shared.clone(),
            channels: 2,
            rate: 1000,
            consumed: 0,
            silence: 0,
            scratch: [0.0; 4096],
        };
        let mut out = [0.0_f32; 16];
        mixer.render(&mut out, Duration::ZERO);
        shared.eof.store(true, Ordering::Release);
        mixer.render(&mut out, Duration::ZERO);
        assert!(shared.ended.load(Ordering::Acquire));
        assert_eq!(shared.played.load(Ordering::Acquire), 8);
        mixer.render(&mut out, Duration::ZERO);
        assert_eq!(shared.played.load(Ordering::Acquire), 8);
    }
}
