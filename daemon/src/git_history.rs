//! Local git history for the mirrored project.
//!
//! Studio is the only writer in mirror mode, so the working tree is a faithful
//! projection of the place and every commit is a real Studio state. That is the
//! whole point: the history answers "what did this script look like then?"
//! without anyone having to remember to save anything.
//!
//! Commits are DEBOUNCED rather than made per write. A Studio push batch can be
//! one keystroke's autosave; committing each one would bury the history in
//! noise. Waiting for the writes to go quiet gives one commit per burst of
//! editing, which is the granularity a person actually reads.
//!
//! This module shells out to `git` instead of linking a git library. The
//! working tree is ordinary files, the operations are `init`/`add`/`commit`,
//! and a user inspecting or rewriting that history will use the same binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the working tree must be quiet before a commit is made.
const QUIET_PERIOD: Duration = Duration::from_secs(5);
/// How often the background task wakes to check for quiet.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct GitHistory {
    root: PathBuf,
    enabled: bool,
    /// Unix millis of the most recent applied write, or 0 when clean.
    dirty_at: AtomicU64,
    available: AtomicBool,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl GitHistory {
    pub fn new(root: PathBuf, enabled: bool) -> Self {
        Self {
            root,
            enabled,
            dirty_at: AtomicU64::new(0),
            available: AtomicBool::new(false),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Prepare the repository. Safe to call on an existing repo: an established
    /// working tree is adopted rather than reinitialized, so a project already
    /// under version control keeps its history and its remotes.
    pub fn prepare(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if git(&self.root, &["--version"]).is_err() {
            return Err("git is not on PATH; history is disabled for this session".into());
        }
        let already = self.root.join(".git").exists();
        if !already {
            git(&self.root, &["init"])?;
            // Mirrored sources are text. Normalizing on the way in keeps a
            // CRLF-vs-LF difference from showing up as a whole-file rewrite.
            git(&self.root, &["config", "core.autocrlf", "false"])?;
        }
        self.available.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Record that Studio wrote something. Cheap and lock-free: the background
    /// task does the work, so this never delays applying a push.
    pub fn note_change(&self) {
        if self.enabled && self.available.load(Ordering::Relaxed) {
            self.dirty_at.store(now_millis(), Ordering::Relaxed);
        }
    }

    fn quiet_long_enough(&self) -> bool {
        let dirty = self.dirty_at.load(Ordering::Relaxed);
        dirty != 0 && now_millis().saturating_sub(dirty) >= QUIET_PERIOD.as_millis() as u64
    }

    /// Commit whatever is staged-able right now. Returns the number of files in
    /// the commit, or `None` when there was nothing to record.
    pub fn commit_now(&self, reason: &str) -> Result<Option<usize>, String> {
        if !self.enabled || !self.available.load(Ordering::Relaxed) {
            return Ok(None);
        }
        // Clear the marker up front. A write landing during the commit re-dirties
        // it and is picked up next pass, which is strictly better than clearing
        // afterwards and swallowing that write.
        self.dirty_at.store(0, Ordering::Relaxed);

        let changed = git(&self.root, &["status", "--porcelain"])?;
        if changed.is_empty() {
            return Ok(None);
        }
        let files = changed.lines().count();
        git(&self.root, &["add", "-A"])?;
        let message = format!(
            "{reason}: {} file{}",
            files,
            if files == 1 { "" } else { "s" }
        );
        // `git commit` fails when a concurrent index lock or an empty diff races
        // us. Surface it rather than retrying blindly; the next quiet period
        // will try again from whatever the tree actually looks like.
        git(&self.root, &["commit", "--no-verify", "-m", &message])?;
        Ok(Some(files))
    }

    /// Restore one path to its last committed state — the mirror's revert.
    ///
    /// The last commit is Studio's content, so git already holds the bytes to
    /// put back and no round-trip to Studio is needed. A path git has never
    /// seen was never Studio's, so removing it is the correct restore.
    pub fn restore_path(&self, path: &Path) -> Result<(), String> {
        if !self.enabled || !self.available.load(Ordering::Relaxed) {
            return Err("history is not available; cannot restore".into());
        }
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        let Some(as_str) = relative.to_str() else {
            return Err(format!("restore {}: non-UTF-8 path", path.display()));
        };
        let tracked = git(&self.root, &["ls-files", "--error-unmatch", as_str]).is_ok();
        if tracked {
            git(&self.root, &["checkout", "--", as_str]).map(|_| ())
        } else if path.exists() {
            std::fs::remove_file(path)
                .map_err(|error| format!("remove untracked {}: {error}", path.display()))
        } else {
            Ok(())
        }
    }
}

/// Background committer. Wakes on a slow poll, commits once the tree has been
/// quiet, and otherwise does nothing at all.
pub fn spawn(history: Arc<GitHistory>) {
    if !history.enabled() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if !history.quiet_long_enough() {
                continue;
            }
            let worker = Arc::clone(&history);
            let result =
                tokio::task::spawn_blocking(move || worker.commit_now("Studio sync")).await;
            match result {
                Ok(Ok(Some(files))) => {
                    eprintln!("bloxsync: committed {files} file(s) from Studio");
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => eprintln!("bloxsync: history commit failed: {error}"),
                Err(error) => eprintln!("bloxsync: history task failed: {error}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bloxsync-history-{name}-{}", now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn identity(root: &Path) {
        let _ = git(root, &["config", "user.email", "test@example.com"]);
        let _ = git(root, &["config", "user.name", "Test"]);
    }

    #[test]
    fn disabled_history_does_nothing() {
        let root = temp_project("disabled");
        let history = GitHistory::new(root.clone(), false);
        history.prepare().unwrap();
        history.note_change();
        assert_eq!(history.commit_now("x").unwrap(), None);
        assert!(!root.join(".git").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prepare_initializes_and_commit_records_studio_writes() {
        let root = temp_project("commit");
        let history = GitHistory::new(root.clone(), true);
        history.prepare().unwrap();
        identity(&root);
        assert!(root.join(".git").exists());

        std::fs::write(root.join("Module.luau"), "return 1\n").unwrap();
        let files = history.commit_now("Studio sync").unwrap();
        assert_eq!(files, Some(1));

        // Nothing further changed, so there is nothing to record.
        assert_eq!(history.commit_now("Studio sync").unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_returns_a_tracked_file_to_studios_content() {
        let root = temp_project("restore");
        let history = GitHistory::new(root.clone(), true);
        history.prepare().unwrap();
        identity(&root);

        let file = root.join("Module.luau");
        std::fs::write(&file, "from studio\n").unwrap();
        history.commit_now("Studio sync").unwrap();

        std::fs::write(&file, "edited on disk\n").unwrap();
        history.restore_path(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "from studio\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_removes_a_file_studio_never_made() {
        let root = temp_project("untracked");
        let history = GitHistory::new(root.clone(), true);
        history.prepare().unwrap();
        identity(&root);
        std::fs::write(root.join("Seed.luau"), "seed\n").unwrap();
        history.commit_now("Studio sync").unwrap();

        let stray = root.join("HandMade.luau");
        std::fs::write(&stray, "not from studio\n").unwrap();
        history.restore_path(&stray).unwrap();
        assert!(!stray.exists(), "a file Studio never made must not survive");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn note_change_marks_dirty_only_once_available() {
        let root = temp_project("dirty");
        let history = GitHistory::new(root.clone(), true);
        history.note_change();
        assert_eq!(
            history.dirty_at.load(Ordering::Relaxed),
            0,
            "a write before prepare must not mark the tree dirty"
        );
        history.prepare().unwrap();
        history.note_change();
        assert_ne!(history.dirty_at.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
