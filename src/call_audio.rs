//! Host-owned PulseAudio capture/playback. PCM travels over a private Unix socket.
use anyhow::Result;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct AudioHost {
    session: Arc<Mutex<Option<AudioSession>>>,
}
struct AudioSession {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}
impl AudioHost {
    pub fn start(&self, path: &Path, input: &str, output: &str) -> Result<()> {
        self.stop();
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let input = input.to_owned();
        let output = output.to_owned();
        let path = path.to_owned();
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            // Remove this listener on every exit, before a replacement is bound.
            struct SocketFile(std::path::PathBuf);
            impl Drop for SocketFile {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _socket_file = SocketFile(path);
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
            let result = supervise(
                socket,
                stop,
                move || {
                    Command::new("ffmpeg")
                        .args([
                            "-v", "error", "-f", "pulse", "-i", &input, "-ac", "1", "-ar", "48000",
                            "-f", "s16le", "pipe:1",
                        ])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                },
                move || {
                    Command::new("ffmpeg")
                        .args([
                            "-v", "error", "-f", "s16le", "-ar", "48000", "-ac", "1", "-i",
                            "pipe:0", "-f", "pulse", &output,
                        ])
                        .stdin(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                },
            );
            if let Err(e) = result {
                tracing::warn!("call audio: {e}");
            }
        });
        *self.session.lock().unwrap() = Some(AudioSession {
            stop: stopped,
            thread,
        });
        Ok(())
    }
    pub fn stop(&self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            session.stop.store(true, Ordering::SeqCst);
            let _ = session.thread.join();
        }
    }
}

type Process = Arc<Mutex<Option<Child>>>;

fn reap(process: &Process) {
    if let Some(mut child) = process.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}
fn retry_pause(stop: &AtomicBool) {
    for _ in 0..10 {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Keep the PCM socket alive across local process failures. Only socket EOF/error
/// or explicit cancellation ends the call. Restarting one direction does not
/// interrupt the other; the protocol's existing silence keepalive covers capture gaps.
fn supervise<C, P>(socket: UnixStream, stop: Arc<AtomicBool>, capture: C, playback: P) -> Result<()>
where
    C: Fn() -> io::Result<Child> + Send + 'static,
    P: Fn() -> io::Result<Child> + Send + 'static,
{
    let mic: Process = Arc::default();
    let speaker: Process = Arc::default();
    let mut outgoing = socket.try_clone()?;
    let mut incoming = socket.try_clone()?;
    let mic_slot = mic.clone();
    let capture_stop = stop.clone();
    let capture_thread = std::thread::spawn(move || {
        while !capture_stop.load(Ordering::SeqCst) {
            let child = capture();
            match child {
                Ok(mut child) => {
                    let Some(mut pipe) = child.stdout.take() else {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    };
                    *mic_slot.lock().unwrap() = Some(child);
                    let mut bytes = [0u8; 1920];
                    while !capture_stop.load(Ordering::SeqCst) {
                        match pipe.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(n) => {
                                if outgoing.write_all(&bytes[..n]).is_err() {
                                    capture_stop.store(true, Ordering::SeqCst);
                                    break;
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    reap(&mic_slot);
                }
                Err(e) => tracing::warn!("capture process restart: {e}"),
            }
            retry_pause(&capture_stop);
        }
        capture_stop.store(true, Ordering::SeqCst);
    });
    let speaker_slot = speaker.clone();
    let play_stop = stop.clone();
    let play_thread = std::thread::spawn(move || {
        let mut pipe = None;
        let mut bytes = [0u8; 1920];
        while !play_stop.load(Ordering::SeqCst) {
            let n = match incoming.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut offset = 0;
            while offset < n && !play_stop.load(Ordering::SeqCst) {
                if pipe.is_none() {
                    match playback() {
                        Ok(mut child) => {
                            pipe = child.stdin.take();
                            *speaker_slot.lock().unwrap() = Some(child);
                            if pipe.is_none() {
                                reap(&speaker_slot);
                                retry_pause(&play_stop);
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("playback process restart: {e}");
                            retry_pause(&play_stop);
                            continue;
                        }
                    }
                }
                match pipe.as_mut().unwrap().write(&bytes[offset..n]) {
                    Ok(0) | Err(_) => {
                        pipe = None;
                        reap(&speaker_slot);
                        retry_pause(&play_stop);
                    }
                    Ok(written) => offset += written,
                }
            }
        }
        drop(pipe);
        reap(&speaker_slot);
        play_stop.store(true, Ordering::SeqCst);
    });
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = socket.shutdown(std::net::Shutdown::Both);
    // Kill blocked pipe I/O before joining, including children installed during cancellation.
    while !capture_thread.is_finished() || !play_thread.is_finished() {
        for process in [&mic, &speaker] {
            if let Some(child) = process.lock().unwrap().as_mut() {
                let _ = child.kill();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = capture_thread.join();
    let _ = play_thread.join();
    reap(&mic);
    reap(&speaker);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    #[test]
    fn process_failures_restart_both_directions_without_reconnecting_socket() {
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let captures = Arc::new(AtomicUsize::new(0));
        let plays = Arc::new(AtomicUsize::new(0));
        let path = std::env::temp_dir().join(format!("line-audio-recovery-{}", std::process::id()));
        let capture_count = captures.clone();
        let play_count = plays.clone();
        let output = path.clone();
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            supervise(
                server,
                stopped,
                move || {
                    let attempt = capture_count.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        return Err(io::Error::other("temporary spawn failure"));
                    }
                    Command::new("sh")
                        .args([
                            "-c",
                            if attempt == 1 {
                                "printf A"
                            } else {
                                "printf B; exec cat"
                            },
                        ])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .spawn()
                },
                move || {
                    if play_count.fetch_add(1, Ordering::SeqCst) == 0 {
                        let mut child = Command::new("sh")
                            .args(["-c", "exit 1"])
                            .stdin(Stdio::piped())
                            .spawn()?;
                        child.wait()?;
                        return Ok(child);
                    }
                    Command::new("cat")
                        .stdin(Stdio::piped())
                        .stdout(std::fs::File::create(&output)?)
                        .spawn()
                },
            )
        });
        // Always stop/reap the supervisor even if an assertion below fails.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut bytes = [0; 2];
            peer.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"AB");
            peer.write_all(b"PCM survives playback restart").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if std::fs::read(&path).unwrap_or_default() == b"PCM survives playback restart" {
                    break;
                }
                assert!(Instant::now() < deadline, "playback failed to recover");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!stop.load(Ordering::SeqCst));
            assert_eq!(captures.load(Ordering::SeqCst), 3);
            assert_eq!(plays.load(Ordering::SeqCst), 2);
        }));
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_file(path);
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn stopping_during_spawn_retry_finishes_without_a_peer_disconnect() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (started, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            supervise(
                socket,
                stopped,
                move || {
                    let _ = started.send(());
                    Err(io::Error::other("device unavailable"))
                },
                || Err(io::Error::other("device unavailable")),
            )
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let began = Instant::now();
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap().unwrap();
        assert!(began.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn replacing_unconnected_listener_does_not_unlink_new_socket() {
        let path =
            std::env::temp_dir().join(format!("line-audio-listener-{}.sock", std::process::id()));
        let host = AudioHost::default();
        host.start(&path, "default", "default").unwrap();
        host.start(&path, "default", "default").unwrap();
        assert!(path.exists());
        host.stop();
        assert!(!path.exists());
    }
}
