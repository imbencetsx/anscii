use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct TestVideo {
    pub folder: PathBuf,
    pub path: PathBuf,
}

impl TestVideo {
    pub fn available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    pub fn new() -> Self {
        let folder = std::env::temp_dir().join(format!(
            "anscii-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("clip with spaces.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=24",
                "-t",
                "2",
                "-c:v",
                "mpeg4",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        Self { folder, path }
    }
}

impl Drop for TestVideo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.folder);
    }
}
