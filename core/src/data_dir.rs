//! Preserve existing absolute cache paths when adopting the new application name.
use std::io;
use std::path::{Path, PathBuf};

pub fn resolve(base: &Path, legacy_flatpak: Option<&Path>) -> io::Result<PathBuf> {
    let current = base.join("line-rust");
    // Never replace or merge a user's existing new-profile directory.
    match std::fs::symlink_metadata(&current) {
        Ok(_) => return current.canonicalize(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let legacy = base.join("line-gtk");
    let source = [Some(legacy.as_path()), legacy_flatpak]
        .into_iter()
        .flatten()
        .find(|path| path.is_dir());
    if let Some(source) = source {
        let source = source.canonicalize()?;
        std::fs::create_dir_all(base)?;
        // Keep SQLite/JSON absolute media paths valid and avoid copying auth
        // and a potentially large history. Both names refer to the same data.
        #[cfg(unix)]
        {
            match std::os::unix::fs::symlink(&source, &current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            return current.canonicalize();
        }
        #[cfg(not(unix))]
        return Ok(source);
    }
    Ok(current)
}
