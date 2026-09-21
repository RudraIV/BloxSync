//! Keeps a daemon alive for every project you have opened.
//!
//! This is the "no steps" half of BloxSync. One supervisor starts at boot and
//! from then on every registered project has a daemon whenever the machine is
//! on, so opening a place in Studio finds a listener already waiting instead of
//! requiring anyone to start anything.
//!
//! Registration is AUTOMATIC: a project registers itself the first time a
//! daemon serves it. Opening a place once is the only step there is, and it is
//! a step you were taking anyway.
//!
//! The loop deliberately does very little. It asks the existing lifecycle code
//! whether a daemon is healthy and starts one when it is not, rather than
//! reimplementing process management. Everything it knows about liveness comes
//! from `daemon_manager::daemon_status`, which already distinguishes running,
//! stale, unresponsive and externally managed — distinctions a supervisor must
//! respect or it will fight the thing it is supposed to be helping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::DaemonStartArgs;
use crate::daemon_manager::{daemon_start, daemon_status};
use crate::lifecycle;

pub const REGISTRY_FILE: &str = "supervised.json";

/// How often the loop checks. Slow on purpose: a daemon that is up needs no
/// attention, and a daemon that just died is not helped by being restarted a
/// hundred milliseconds sooner.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
/// A project whose daemon will not stay up is retried with a widening gap
/// rather than hammered. Studio not being open is a perfectly ordinary reason
/// for a start to fail, and that state can last all day.
const BACKOFF_BASE: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(10 * 60);
const START_TIMEOUT_SECONDS: f64 = 30.0;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub projects: Vec<PathBuf>,
}

fn registry_path(state_dir: &Path) -> PathBuf {
    state_dir.join(REGISTRY_FILE)
}

pub fn load_registry(state_dir: &Path) -> std::io::Result<Registry> {
    let path = registry_path(state_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(error),
    }
}

fn save_registry(state_dir: &Path, registry: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let text = serde_json::to_string_pretty(registry)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let path = registry_path(state_dir);
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text + "\n")?;
    crate::lifecycle::replace_file_atomic(&temp, &path)
}

/// Add a project, returning whether it was newly registered.
///
/// Called by `serve` on every start, so it must be cheap and idempotent: the
/// common case is a project that is already present and nothing is written.
pub fn register(state_dir: &Path, canonical_project: &Path) -> std::io::Result<bool> {
    let mut registry = load_registry(state_dir)?;
    if registry
        .projects
        .iter()
        .any(|existing| existing == canonical_project)
    {
        return Ok(false);
    }
    registry.projects.push(canonical_project.to_path_buf());
    save_registry(state_dir, &registry)?;
    Ok(true)
}

pub fn unregister(state_dir: &Path, canonical_project: &Path) -> std::io::Result<bool> {
    let mut registry = load_registry(state_dir)?;
    let before = registry.projects.len();
    registry
        .projects
        .retain(|existing| existing != canonical_project);
    if registry.projects.len() == before {
        return Ok(false);
    }
    save_registry(state_dir, &registry)?;
    Ok(true)
}

/// What the loop decided to do about one project on one pass. Separated from
/// the doing so it can be tested without spawning anything.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Healthy, or someone else's to manage.
    Leave(&'static str),
    /// Start a daemon.
    Start,
    /// Failed recently; wait longer before trying again.
    Backoff,
}

#[derive(Debug, Default)]
struct Failures {
    counts: HashMap<PathBuf, u32>,
    next_attempt: HashMap<PathBuf, std::time::Instant>,
}

impl Failures {
    fn ready(&self, project: &Path) -> bool {
        self.next_attempt
            .get(project)
            .is_none_or(|at| std::time::Instant::now() >= *at)
    }

    fn succeeded(&mut self, project: &Path) {
        self.counts.remove(project);
        self.next_attempt.remove(project);
    }

    fn failed(&mut self, project: &Path) -> Duration {
        let count = self.counts.entry(project.to_path_buf()).or_insert(0);
        *count = count.saturating_add(1);
        let delay = backoff_delay(*count);
        self.next_attempt
            .insert(project.to_path_buf(), std::time::Instant::now() + delay);
        delay
    }
}

/// Doubling backoff, capped. Exposed for tests because an unbounded or
/// non-increasing delay here is the difference between a quiet background
/// process and one that respawns a broken daemon forever.
pub fn backoff_delay(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(16);
    let scaled = BACKOFF_BASE.saturating_mul(1u32 << exponent);
    scaled.min(BACKOFF_MAX)
}

/// Decide what to do about a project, given how its daemon currently looks.
pub fn decide(
    running: bool,
    stale: bool,
    unresponsive: bool,
    externally_managed: bool,
    ready: bool,
) -> Action {
    // An unresponsive daemon still owns its port and its record. Starting a
    // second one would be a duplicate, which the lifecycle code goes out of its
    // way to avoid; leave it and let the next pass look again.
    if unresponsive {
        return Action::Leave("unresponsive");
    }
    if externally_managed {
        return Action::Leave("externally managed");
    }
    if running && !stale {
        return Action::Leave("running");
    }
    if !ready {
        return Action::Backoff;
    }
    Action::Start
}

/// Re-launch this process with no console and exit.
///
/// The interactive autostart fallback runs a console binary on the desktop, so
/// a window would appear and stay. Handing the real work to a detached child
/// reduces that to the launcher's own brief blink, and the supervisor itself is
/// never drawn at all.
#[cfg(windows)]
pub fn respawn_detached(interval: Option<f64>, state_dir: Option<&Path>) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let executable =
        std::env::current_exe().map_err(|error| format!("locate this executable: {error}"))?;
    let mut command = std::process::Command::new(executable);
    command.arg("supervise").arg("--quiet");
    if let Some(interval) = interval {
        command.arg("--interval").arg(interval.to_string());
    }
    if let Some(state_dir) = state_dir {
        command.arg("--data-dir").arg(state_dir);
    }
    command
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| format!("detach supervisor: {error}"))
}

#[cfg(not(windows))]
pub fn respawn_detached(_interval: Option<f64>, _state_dir: Option<&Path>) -> Result<u32, String> {
    Err("--detach is only implemented for Windows so far".into())
}

/// Run until killed. One pass per interval over every registered project.
pub async fn run(state_dir: PathBuf, interval: Option<f64>, quiet: bool) -> Result<(), String> {
    let interval = interval
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT_INTERVAL);
    let mut failures = Failures::default();

    if !quiet {
        eprintln!(
            "bloxsync: supervising projects listed in {}",
            registry_path(&state_dir).display()
        );
    }

    loop {
        // Re-read every pass rather than caching: a project registered by a
        // daemon that started a moment ago should be picked up without anyone
        // restarting the supervisor.
        let registry = load_registry(&state_dir).unwrap_or_default();
        for project in registry.projects.clone() {
            if let Err(error) = supervise_one(&state_dir, &project, &mut failures, quiet).await {
                if !quiet {
                    eprintln!("bloxsync: {}: {error}", project.display());
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn supervise_one(
    state_dir: &Path,
    project: &Path,
    failures: &mut Failures,
    quiet: bool,
) -> Result<(), String> {
    let canonical =
        lifecycle::canonical_project(project).map_err(|error| format!("canonicalize: {error}"))?;
    let paths = lifecycle::runtime_paths(state_dir.to_path_buf(), &canonical);

    let status = daemon_status(&canonical, &paths, false).map_err(|error| error.to_string())?;
    let action = decide(
        status.running,
        status.stale,
        status.unresponsive,
        status.externally_managed,
        failures.ready(&canonical),
    );

    match action {
        Action::Leave(_) => {
            failures.succeeded(&canonical);
            Ok(())
        }
        Action::Backoff => Ok(()),
        Action::Start => {
            let previous_port = lifecycle::read_record(&paths.record)
                .ok()
                .flatten()
                .map(|record| record.port);
            let started = daemon_start(DaemonStartArgs {
                project: canonical.clone(),
                port: previous_port,
                managed_by: "supervisor".to_string(),
                owner_token: None,
                owner_token_env: None,
                game_id: None,
                group_id: None,
                place_id: Vec::new(),
                projects_root: None,
                data_dir: Some(state_dir.to_path_buf()),
                timeout: START_TIMEOUT_SECONDS,
                parent_stdin_lease: false,
                raw: true,
            })
            .await;

            match started {
                Ok(status) if status.running => {
                    failures.succeeded(&canonical);
                    if !quiet {
                        eprintln!(
                            "bloxsync: started daemon for {} on port {}",
                            canonical.display(),
                            status.port.unwrap_or(0)
                        );
                    }
                    Ok(())
                }
                Ok(_) => {
                    let delay = failures.failed(&canonical);
                    Err(format!(
                        "daemon did not come up; retrying in {}s",
                        delay.as_secs()
                    ))
                }
                Err(error) => {
                    let delay = failures.failed(&canonical);
                    Err(format!("{error}; retrying in {}s", delay.as_secs()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bloxsync-sup-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn registry_round_trips_and_register_is_idempotent() {
        let state = scratch("registry");
        let project = PathBuf::from("/projects/Mainline");
        assert!(load_registry(&state).unwrap().projects.is_empty());

        assert!(register(&state, &project).unwrap(), "first add registers");
        assert!(
            !register(&state, &project).unwrap(),
            "a project already present must not be added twice"
        );
        assert_eq!(
            load_registry(&state).unwrap().projects,
            vec![project.clone()]
        );

        assert!(unregister(&state, &project).unwrap());
        assert!(!unregister(&state, &project).unwrap());
        assert!(load_registry(&state).unwrap().projects.is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_missing_registry_is_empty_not_an_error() {
        let state = scratch("missing");
        let _ = std::fs::remove_dir_all(&state);
        assert!(load_registry(&state).unwrap().projects.is_empty());
    }

    #[test]
    fn a_corrupt_registry_does_not_take_the_supervisor_down() {
        let state = scratch("corrupt");
        std::fs::write(registry_path(&state), "{ not json").unwrap();
        assert!(load_registry(&state).unwrap().projects.is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_healthy_daemon_is_left_alone() {
        assert_eq!(
            decide(true, false, false, false, true),
            Action::Leave("running")
        );
    }

    #[test]
    fn an_unresponsive_daemon_is_never_duplicated() {
        // It still owns its port and record. Starting a second one is exactly
        // the duplicate the lifecycle code refuses to create.
        assert_eq!(
            decide(false, false, true, false, true),
            Action::Leave("unresponsive")
        );
        assert_eq!(
            decide(true, true, true, false, true),
            Action::Leave("unresponsive")
        );
    }

    #[test]
    fn an_externally_managed_daemon_is_not_ours_to_restart() {
        assert_eq!(
            decide(false, false, false, true, true),
            Action::Leave("externally managed")
        );
    }

    #[test]
    fn a_stopped_or_stale_daemon_is_started() {
        assert_eq!(decide(false, false, false, false, true), Action::Start);
        assert_eq!(decide(true, true, false, false, true), Action::Start);
    }

    #[test]
    fn a_recently_failed_project_waits() {
        assert_eq!(decide(false, false, false, false, false), Action::Backoff);
    }

    #[test]
    fn backoff_widens_and_is_capped() {
        assert_eq!(backoff_delay(1), BACKOFF_BASE);
        assert_eq!(backoff_delay(2), BACKOFF_BASE * 2);
        assert_eq!(backoff_delay(3), BACKOFF_BASE * 4);
        assert_eq!(
            backoff_delay(99),
            BACKOFF_MAX,
            "must not grow without bound"
        );
        assert!(backoff_delay(50) <= BACKOFF_MAX);
    }

    #[test]
    fn failures_widen_then_reset_on_success() {
        let mut failures = Failures::default();
        let project = PathBuf::from("/projects/X");
        assert!(failures.ready(&project), "an unseen project is ready");

        let first = failures.failed(&project);
        let second = failures.failed(&project);
        assert!(second > first, "consecutive failures must widen the gap");
        assert!(!failures.ready(&project), "a just-failed project waits");

        failures.succeeded(&project);
        assert!(failures.ready(&project), "success clears the backoff");
    }
}
