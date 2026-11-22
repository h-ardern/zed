use anyhow::{Context as _, Result};
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The name Claude Code shows in its `/ide` picker.
pub const IDE_NAME: &str = "Ded";

/// The discovery file Claude Code reads from `~/.claude/ide/<port>.lock`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    pub pid: u32,
    pub workspace_folders: Vec<String>,
    pub ide_name: String,
    pub transport: String,
    pub auth_token: String,
}

impl LockFile {
    pub fn new(workspace_folders: Vec<String>, auth_token: String) -> Self {
        Self {
            pid: std::process::id(),
            workspace_folders,
            ide_name: IDE_NAME.to_string(),
            transport: "ws".to_string(),
            auth_token,
        }
    }
}

/// Claude Code honours `CLAUDE_CONFIG_DIR`, so the lock files must follow it.
pub fn lock_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("ide"),
        None => paths::home_dir().join(".claude").join("ide"),
    }
}

pub fn lock_path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("{port}.lock"))
}

/// A 128-bit token from the OS random source, as lowercase hex.
pub fn generate_auth_token() -> String {
    let mut rng = StdRng::from_os_rng();
    let bytes: [u8; 16] = rng.random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn write(path: &Path, lock_file: &LockFile) -> Result<()> {
    let dir = path.parent().context("lock file path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating Claude Code lock dir {}", dir.display()))?;
    let contents = serde_json::to_string(lock_file)?;

    // Write then rename, so Claude never reads a half-written file.
    let temp_path = path.with_extension("lock.tmp");
    std::fs::write(&temp_path, contents)
        .with_context(|| format!("writing {}", temp_path.display()))?;
    restrict_permissions(&temp_path)?;
    std::fs::rename(&temp_path, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "failed to remove Claude Code lock file {}: {error}",
            path.display()
        );
    }
}

/// Removes lock files left behind by Ded processes that have exited, so Claude's
/// `/ide` picker doesn't offer dead windows.
pub fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("lock") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(lock_file) = serde_json::from_str::<LockFile>(&contents) else {
            continue;
        };
        if lock_file.ide_name == IDE_NAME && !process_is_alive(lock_file.pid) {
            remove(&path);
        }
    }
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

// Without a cheap liveness check, keep the file rather than delete a live window's.
#[cfg(not(target_os = "linux"))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_file_round_trip() -> Result<()> {
        let dir = tempfile_dir()?;
        let path = lock_path(&dir, 12345);
        let lock_file = LockFile::new(vec!["/home/user/project".into()], generate_auth_token());

        write(&path, &lock_file)?;
        let contents = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&contents)?;
        assert_eq!(parsed["ideName"], IDE_NAME);
        assert_eq!(parsed["transport"], "ws");
        assert_eq!(parsed["workspaceFolders"][0], "/home/user/project");
        assert_eq!(serde_json::from_str::<LockFile>(&contents)?, lock_file);

        remove(&path);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_auth_token_is_32_hex_chars() {
        let token = generate_auth_token();
        assert_eq!(token.len(), 32);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(token, generate_auth_token());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_remove_stale_keeps_live_and_removes_dead() -> Result<()> {
        let dir = tempfile_dir()?;
        let live = lock_path(&dir, 1);
        write(&live, &LockFile::new(vec![], generate_auth_token()))?;

        let dead = lock_path(&dir, 2);
        let mut dead_lock = LockFile::new(vec![], generate_auth_token());
        dead_lock.pid = u32::MAX;
        write(&dead, &dead_lock)?;

        remove_stale(&dir);
        assert!(live.exists());
        assert!(!dead.exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn tempfile_dir() -> Result<PathBuf> {
        let dir =
            std::env::temp_dir().join(format!("ded-claude-ide-test-{}", generate_auth_token()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
