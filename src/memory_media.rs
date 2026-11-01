//! Anonymous RAM-backed media. No temporary/cache files or SQLite media blobs.
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Registry = HashMap<String, Weak<File>>;
fn registry() -> &'static Mutex<Registry> {
    static FILES: OnceLock<Mutex<Registry>> = OnceLock::new();
    FILES.get_or_init(Mutex::default)
}

#[derive(Clone)]
pub struct RamFile(pub Arc<File>);
impl RamFile {
    pub fn new() -> Result<Self> {
        // CLOEXEC: children read via the parent's /proc path, not inherited FDs.
        let fd = unsafe { libc::memfd_create(c"line-media".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = Self(Arc::new(unsafe { File::from_raw_fd(fd) }));
        let mut files = registry().lock().unwrap();
        files.retain(|_, file| file.strong_count() > 0);
        files.insert(file.path(), Arc::downgrade(&file.0));
        Ok(file)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let file = Self::new()?;
        file.write(bytes)?;
        Ok(file)
    }
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        (&*self.0).write_all(bytes)?;
        Ok(())
    }
    pub fn path(&self) -> String {
        format!("/proc/{}/fd/{}", std::process::id(), self.0.as_raw_fd())
    }
}

pub fn is_ram_path(path: &str) -> bool {
    path.starts_with(&format!("/proc/{}/fd/", std::process::id()))
}
pub fn lease(path: &str) -> Option<Arc<File>> {
    registry().lock().ok()?.get(path)?.upgrade()
}

pub fn stage_clipboard(bytes: &[u8]) -> Result<String> {
    let file = RamFile::from_bytes(bytes)?;
    let path = file.path();
    uploads().lock().unwrap().insert(path.clone(), file);
    Ok(path)
}
fn uploads() -> &'static Mutex<HashMap<String, RamFile>> {
    static UPLOADS: OnceLock<Mutex<HashMap<String, RamFile>>> = OnceLock::new();
    UPLOADS.get_or_init(Mutex::default)
}
pub fn take_upload(path: &str) -> Option<RamFile> {
    uploads().lock().unwrap().remove(path)
}

#[derive(Clone)]
pub struct Media {
    pub original: RamFile,
    pub preview: Option<RamFile>,
}
impl Media {
    pub fn apply(&self, row: &serde_json::Value) -> serde_json::Value {
        let mut row = row.clone();
        row["filePath"] = self.original.path().into();
        row["imagePath"] = self.preview.as_ref().map(RamFile::path).into();
        row["needsMedia"] = false.into();
        row
    }
}

#[derive(Default)]
struct State {
    policies: BTreeMap<String, BTreeSet<String>>,
    account: String,
    generation: u64,
    epoch: u64,
    chat: String,
    visible: BTreeSet<String>,
    entries: HashMap<String, Media>,
}

#[derive(Default)]
pub struct MemoryMedia(Mutex<State>);
pub fn for_data(data: &std::path::Path) -> Arc<MemoryMedia> {
    static STORES: OnceLock<Mutex<HashMap<std::path::PathBuf, Arc<MemoryMedia>>>> = OnceLock::new();
    STORES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .entry(data.to_owned())
        .or_default()
        .clone()
}
impl MemoryMedia {
    pub fn configure(&self, policies: BTreeMap<String, BTreeSet<String>>) {
        let mut s = self.0.lock().unwrap();
        if s.policies != policies {
            let was_enabled = s
                .policies
                .get(&s.account)
                .is_some_and(|v| v.contains(&s.chat));
            let enabled = policies
                .get(&s.account)
                .is_some_and(|v| v.contains(&s.chat));
            s.policies = policies;
            if was_enabled != enabled {
                s.epoch += 1;
                s.entries.clear();
                s.visible.clear();
            }
        }
    }
    pub fn session(&self, account: &str, generation: u64) {
        let mut s = self.0.lock().unwrap();
        if s.account != account || s.generation != generation {
            s.account = account.into();
            s.generation = generation;
            s.epoch += 1;
            s.chat.clear();
            s.visible.clear();
            s.entries.clear();
        }
    }
    pub fn enabled(&self, account: &str, chat: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .policies
            .get(account)
            .is_some_and(|v| v.contains(chat))
    }
    pub fn current_enabled(&self, chat: &str) -> bool {
        let s = self.0.lock().unwrap();
        s.policies.get(&s.account).is_some_and(|v| v.contains(chat))
    }
    pub fn visible(&self, chat: &str, ids: BTreeSet<String>) -> bool {
        let mut s = self.0.lock().unwrap();
        if s.chat == chat && s.visible == ids {
            return false;
        }
        if s.chat != chat {
            s.entries.clear();
            s.epoch += 1;
        }
        s.chat = chat.into();
        s.visible = ids;
        let visible = s.visible.clone();
        s.entries.retain(|id, _| visible.contains(id));
        true
    }
    pub fn requested(&self, chat: &str, id: &str) -> bool {
        let s = self.0.lock().unwrap();
        s.chat == chat && s.visible.contains(id)
    }
    pub fn ticket(&self, chat: &str, id: &str) -> Result<u64> {
        let s = self.0.lock().unwrap();
        if s.chat != chat || !s.visible.contains(id) {
            bail!("media_not_visible");
        }
        Ok(s.epoch)
    }
    pub fn check(&self, chat: &str, id: &str, epoch: u64) -> Result<()> {
        if self.ticket(chat, id)? != epoch {
            bail!("media_view_changed");
        }
        Ok(())
    }
    pub fn get(&self, chat: &str, id: &str) -> Option<Media> {
        let s = self.0.lock().unwrap();
        (s.chat == chat)
            .then(|| s.entries.get(id).cloned())
            .flatten()
    }
    pub fn insert(&self, chat: &str, id: &str, epoch: u64, media: Media) -> Result<()> {
        let mut s = self.0.lock().unwrap();
        if s.epoch != epoch || s.chat != chat || !s.visible.contains(id) {
            bail!("media_view_changed");
        }
        s.entries.insert(id.into(), media);
        Ok(())
    }
}

pub fn preview(original: &RamFile) -> Result<RamFile> {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            &original.path(),
            "-vf",
            "scale=440:440:force_original_aspect_ratio=decrease",
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .output()?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!("thumbnail generation failed");
    }
    RamFile::from_bytes(&output.stdout).context("memory preview")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_evicts_bytes_and_cancels_old_session_results() {
        let cache = MemoryMedia::default();
        cache.configure(BTreeMap::from([(
            "a".into(),
            BTreeSet::from(["chat".into()]),
        )]));
        cache.session("a", 1);
        assert!(cache.enabled("a", "chat"));
        assert!(!cache.enabled("b", "chat"));
        assert!(cache.ticket("chat", "1").is_err());
        cache.visible("chat", BTreeSet::from(["1".into()]));
        let ticket = cache.ticket("chat", "1").unwrap();
        let original = RamFile::from_bytes(b"only in memory").unwrap();
        let path = original.path();
        assert_eq!(std::fs::read(&path).unwrap(), b"only in memory");
        let viewer = lease(&path).unwrap();
        cache
            .insert(
                "chat",
                "1",
                ticket,
                Media {
                    original,
                    preview: None,
                },
            )
            .unwrap();
        cache.visible("chat", BTreeSet::from(["1".into(), "2".into()]));
        cache.check("chat", "1", ticket).unwrap();
        cache.visible("chat", BTreeSet::from(["2".into()]));
        assert!(cache.get("chat", "1").is_none());
        assert!(cache.check("chat", "1", ticket).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"only in memory");
        drop(viewer);
        assert!(lease(&path).is_none());
        assert!(!std::path::Path::new(&path).exists());
        cache.session("b", 2);
        assert!(cache.ticket("chat", "2").is_err());
        assert!(!cache.current_enabled("chat"));
    }
}
