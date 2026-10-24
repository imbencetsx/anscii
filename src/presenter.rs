//! A single in-flight screen. Slow terminals never block input/audio or build a
//! queue of obsolete frames. The UI composes again only when the sink is ready.
use crate::render::{Grid, Renderer};
use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Default)]
struct State {
    pending: Option<Grid>,
    recycled: Option<Grid>,
    busy: bool,
    error: Option<io::Error>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
}

pub struct Presenter {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Presenter {
    pub fn new() -> io::Result<Self> {
        let output = sink::Output::new(io::stdout())?;
        let shared = Arc::new(Shared::default());
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("anscii-present".into())
            .spawn(move || {
                if let Err(error) = present(&state, output) {
                    let mut guard = state.state.lock().unwrap();
                    guard.error = Some(error);
                    guard.busy = false;
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn ready(&self) -> bool {
        !self.shared.state.lock().unwrap().busy
    }

    pub fn grid(&self, width: u16, height: u16) -> io::Result<Option<Grid>> {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(error) = state.error.take() {
            return Err(error);
        }
        if state.busy {
            return Ok(None);
        }
        state.busy = true;
        let grid = state
            .recycled
            .take()
            .filter(|g| (g.width, g.height) == (width, height))
            .unwrap_or_else(|| Grid::new(width, height));
        Ok(Some(grid))
    }

    pub fn submit(&self, grid: Grid) {
        let mut state = self.shared.state.lock().unwrap();
        debug_assert!(state.pending.is_none() && state.busy);
        state.pending = Some(grid);
        self.shared.wake.notify_one();
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn present(shared: &Shared, mut output: sink::Output<io::Stdout>) -> io::Result<()> {
    let mut renderer = Renderer::new();
    loop {
        let grid = {
            let mut state = shared.state.lock().unwrap();
            while state.pending.is_none() && !shared.stop.load(Ordering::Acquire) {
                state = shared.wake.wait(state).unwrap();
            }
            if shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            state.pending.take().unwrap()
        };
        if !output.write(renderer.encode(&grid), &shared.stop, None)? {
            return Ok(());
        }
        let mut state = shared.state.lock().unwrap();
        state.recycled = Some(grid);
        state.busy = false;
    }
}

/// Cleanup must restore raw mode even if the terminal stops reading entirely.
pub fn restore(bytes: &[u8]) {
    if let Ok(mut output) = sink::Output::new(io::stdout()) {
        let _ = output.write(
            bytes,
            &AtomicBool::new(false),
            Some(Instant::now() + Duration::from_millis(100)),
        );
    }
}

#[cfg(unix)]
mod sink {
    use super::*;
    use rustix::{
        event::{self, FdSetElement, Timespec},
        fs::{self, OFlags},
        io::Errno,
    };
    use std::os::fd::{AsFd, AsRawFd};

    pub(super) struct Output<F: AsFd> {
        fd: F,
        flags: OFlags,
        writable: Vec<FdSetElement>,
    }

    impl<F: AsFd> Output<F> {
        pub(super) fn new(fd: F) -> io::Result<Self> {
            let flags = fs::fcntl_getfl(&fd)?;
            fs::fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?;
            let count = event::fd_set_num_elements(1, fd.as_fd().as_raw_fd() + 1);
            Ok(Self {
                fd,
                flags,
                writable: vec![FdSetElement::default(); count],
            })
        }

        pub(super) fn write(
            &mut self,
            mut bytes: &[u8],
            stop: &AtomicBool,
            deadline: Option<Instant>,
        ) -> io::Result<bool> {
            while !bytes.is_empty() {
                if stop.load(Ordering::Acquire) || deadline.is_some_and(|end| Instant::now() >= end)
                {
                    return Ok(false);
                }
                match rustix::io::write(&self.fd, bytes) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(count) => bytes = &bytes[count..],
                    Err(Errno::INTR) => continue,
                    Err(Errno::AGAIN) => {
                        self.writable.fill(FdSetElement::default());
                        let fd = self.fd.as_fd();
                        event::fd_set_insert(&mut self.writable, fd.as_raw_fd());
                        let timeout = Timespec {
                            tv_sec: 0,
                            tv_nsec: 10_000_000,
                        };
                        // SAFETY: the only set bit belongs to `fd`, held open by
                        // self.fd for the call; storage was sized for that fd.
                        let result = unsafe {
                            event::select(
                                fd.as_raw_fd() + 1,
                                None,
                                Some(&mut self.writable),
                                None,
                                Some(&timeout),
                            )
                        };
                        if let Err(error) = result
                            && error != Errno::INTR
                        {
                            return Err(error.into());
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(true)
        }
    }

    impl<F: AsFd> Drop for Output<F> {
        fn drop(&mut self) {
            let _ = fs::fcntl_setfl(&self.fd, self.flags);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::net::UnixStream;

        #[test]
        fn backpressure_is_cancellable_and_restores_descriptor_flags() {
            let (writer, _reader) = UnixStream::pair().unwrap();
            let initial = fs::fcntl_getfl(&writer).unwrap();
            let flags_fd = writer.try_clone().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let cancelled = stop.clone();
            let worker = thread::spawn(move || {
                let mut output = Output::new(writer).unwrap();
                output
                    .write(&vec![0; 8 * 1024 * 1024], &cancelled, None)
                    .unwrap()
            });
            thread::sleep(Duration::from_millis(30));
            let started = Instant::now();
            stop.store(true, Ordering::Release);
            assert!(!worker.join().unwrap());
            assert!(started.elapsed() < Duration::from_secs(1));
            // Darwin may add kernel bookkeeping bits after socket writes;
            // only the mutable nonblocking mode belongs to this guard.
            assert_eq!(
                fs::fcntl_getfl(&flags_fd)
                    .unwrap()
                    .contains(OFlags::NONBLOCK),
                initial.contains(OFlags::NONBLOCK)
            );
        }

        #[test]
        fn partial_writes_deliver_the_exact_stream() {
            use std::io::Read;
            let (writer, mut reader) = UnixStream::pair().unwrap();
            let bytes = vec![b'x'; 512 * 1024];
            let expected = bytes.clone();
            let worker = thread::spawn(move || {
                let mut output = Output::new(writer).unwrap();
                assert!(output.write(&bytes, &AtomicBool::new(false), None).unwrap());
            });
            let mut received = Vec::new();
            reader.read_to_end(&mut received).unwrap();
            worker.join().unwrap();
            assert_eq!(received, expected);
        }
    }
}

#[cfg(not(unix))]
mod sink {
    use super::*;
    use std::io::Write;
    pub(super) struct Output<F> {
        fd: F,
    }
    impl<F: Write> Output<F> {
        pub(super) fn new(fd: F) -> io::Result<Self> {
            Ok(Self { fd })
        }
        pub(super) fn write(
            &mut self,
            bytes: &[u8],
            stop: &AtomicBool,
            _deadline: Option<Instant>,
        ) -> io::Result<bool> {
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            self.fd.write_all(bytes)?;
            self.fd.flush()?;
            Ok(true)
        }
    }
}
