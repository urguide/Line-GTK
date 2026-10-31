//! Host-owned PulseAudio capture/playback. PCM travels over a private Unix socket.
use anyhow::Result;
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct AudioHost {
    stop: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}
impl AudioHost {
    pub fn start(&self, path: &Path, input: &str, output: &str) -> Result<()> {
        self.stop();
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        *self.stop.lock().unwrap() = Some(stop.clone());
        let input = input.to_owned();
        let output = output.to_owned();
        let path = path.to_owned();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if stop.load(Ordering::SeqCst) || started.elapsed().as_secs() > 10 {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            };
            let result = (|| -> Result<()> {
                let mut mic = Command::new("ffmpeg")
                    .args([
                        "-v", "error", "-f", "pulse", "-i", &input, "-ac", "1", "-ar", "48000",
                        "-f", "s16le", "pipe:1",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                let mut speaker = match Command::new("ffmpeg")
                    .args([
                        "-v", "error", "-f", "s16le", "-ar", "48000", "-ac", "1", "-i", "pipe:0",
                        "-f", "pulse", &output,
                    ])
                    .stdin(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(e) => {
                        let _ = mic.kill();
                        let _ = mic.wait();
                        return Err(e.into());
                    }
                };
                let mut capture = mic.stdout.take().unwrap();
                let mut playback = speaker.stdin.take().unwrap();
                let mut incoming = socket.try_clone()?;
                let mut outgoing = socket.try_clone()?;
                let done = stop.clone();
                let capture_thread = std::thread::spawn(move || {
                    let _ = io::copy(&mut capture, &mut outgoing);
                    done.store(true, Ordering::SeqCst);
                });
                let done = stop.clone();
                let play_thread = std::thread::spawn(move || {
                    let _ = io::copy(&mut incoming, &mut playback);
                    done.store(true, Ordering::SeqCst);
                });
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let _ = socket.shutdown(std::net::Shutdown::Both);
                let _ = mic.kill();
                let _ = speaker.kill();
                let _ = mic.wait();
                let _ = speaker.wait();
                let _ = capture_thread.join();
                let _ = play_thread.join();
                Ok(())
            })();
            if let Err(e) = result {
                tracing::warn!("call audio: {e}");
            }
            let _ = std::fs::remove_file(path);
        });
        Ok(())
    }
    pub fn stop(&self) {
        if let Some(stop) = self.stop.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
        }
    }
}
