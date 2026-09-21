//! Start the supervisor when the machine starts, with no window and no steps.
//!
//! On Windows this registers a Task Scheduler logon task rather than a
//! `...\CurrentVersion\Run` entry, for one specific reason: `Run` launches a
//! console-subsystem binary on the interactive desktop and a console window
//! appears. Task Scheduler's `Hidden` setting does NOT fix that — it hides the
//! task from the Task Scheduler list, not the window.
//!
//! What does fix it is `LogonType = S4U` ("run whether the user is logged on or
//! not", without storing a password). An S4U task runs off the interactive
//! desktop, so nothing is ever drawn. The supervisor only speaks loopback HTTP
//! and touches the user's own state directory, so it loses nothing by running
//! non-interactively.
//!
//! REGISTERING an S4U task needs elevation, though, and demanding an admin
//! shell for a per-user convenience is its own kind of step. So install falls
//! back to an interactive task and the supervisor re-launches itself with
//! CREATE_NO_WINDOW; the supervisor is still never drawn, and only the launcher
//! blinks for a fraction of a second at logon.

use std::path::Path;

pub const TASK_NAME: &str = "BloxSync Supervisor";

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Installed { command: String },
    NotInstalled,
}

/// Task Scheduler XML for a hidden, windowless logon task.
///
/// `ExecutionTimeLimit PT0S` means "no limit" — without it Windows kills the
/// supervisor after three days, which would look exactly like a mysterious
/// once-in-a-while failure to sync.
pub fn task_xml(user: &str, command: &str, arguments: &str, logon_type: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Keeps a BloxSync daemon alive for every registered Roblox Studio project.</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>{logon_type}</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// XML escape for the few characters that can appear in a Windows path or in a
/// user name. A path containing `&` would otherwise produce invalid XML and a
/// task that silently fails to register.
pub fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// schtasks wants UTF-16 for `/xml`. A UTF-8 file is rejected with an
/// unhelpful parse error.
pub fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(windows)]
fn current_user() -> Result<String, String> {
    let name = std::env::var("USERNAME").map_err(|_| "USERNAME is not set".to_string())?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Ok(format!("{domain}\\{name}")),
        _ => Ok(name),
    }
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|error| format!("schtasks: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }
    Ok(stdout)
}

/// Which logon type the registered task ended up using.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Registered {
    /// Off the interactive desktop. Nothing is ever drawn. Needs elevation.
    Windowless,
    /// Interactive fallback for a non-elevated install. The supervisor still
    /// ends up windowless because it re-launches itself detached, but the
    /// launcher itself blinks for a fraction of a second at logon.
    InteractiveFallback,
}

#[cfg(windows)]
fn create_task(user: &str, executable: &Path, arguments: &str, logon: &str) -> Result<(), String> {
    let xml = task_xml(
        &escape_xml(user),
        &escape_xml(&executable.display().to_string()),
        &escape_xml(arguments),
        logon,
    );
    let path = std::env::temp_dir().join(format!("bloxsync-task-{}.xml", std::process::id()));
    std::fs::write(&path, utf16le_with_bom(&xml))
        .map_err(|error| format!("write task definition: {error}"))?;
    let result = schtasks(&[
        "/create",
        "/tn",
        TASK_NAME,
        "/xml",
        &path.display().to_string(),
        "/f",
    ]);
    let _ = std::fs::remove_file(&path);
    result.map(|_| ())
}

#[cfg(windows)]
pub fn install(executable: &Path, arguments: &str) -> Result<Registered, String> {
    let user = current_user()?;
    // S4U is the windowless one but registering it needs elevation. Rather than
    // demand an admin shell for what is a per-user convenience, fall back to an
    // interactive task and let the supervisor detach itself.
    match create_task(&user, executable, arguments, "S4U") {
        Ok(()) => Ok(Registered::Windowless),
        Err(error) if error.to_lowercase().contains("access is denied") => {
            create_task(&user, executable, arguments, "InteractiveToken")
                .map(|()| Registered::InteractiveFallback)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub fn uninstall() -> Result<bool, String> {
    match schtasks(&["/delete", "/tn", TASK_NAME, "/f"]) {
        Ok(_) => Ok(true),
        // Not installed is a successful no-op, not a failure.
        Err(error) if error.to_lowercase().contains("cannot find") => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub fn status() -> Result<Status, String> {
    match schtasks(&["/query", "/tn", TASK_NAME, "/xml", "ONE"]) {
        Ok(xml) => {
            let command = extract_tag(&xml, "Command").unwrap_or_default();
            let arguments = extract_tag(&xml, "Arguments").unwrap_or_default();
            Ok(Status::Installed {
                command: format!("{command} {arguments}").trim().to_string(),
            })
        }
        Err(error) if error.to_lowercase().contains("cannot find") => Ok(Status::NotInstalled),
        Err(error) => Err(error),
    }
}

/// Minimal tag reader for reporting back what the installed task runs. This is
/// display only; nothing branches on it.
pub fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

#[cfg(not(windows))]
pub fn install(_executable: &Path, _arguments: &str) -> Result<(), String> {
    Err("autostart is only implemented for Windows so far".into())
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<bool, String> {
    Err("autostart is only implemented for Windows so far".into())
}

#[cfg(not(windows))]
pub fn status() -> Result<Status, String> {
    Err("autostart is only implemented for Windows so far".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_utf16_little_endian_with_a_bom() {
        let bytes = utf16le_with_bom("AB");
        assert_eq!(bytes, vec![0xFF, 0xFE, b'A', 0x00, b'B', 0x00]);
    }

    #[test]
    fn paths_with_xml_significant_characters_are_escaped() {
        assert_eq!(
            escape_xml(r"C:\Tools & Things\a.exe"),
            r"C:\Tools &amp; Things\a.exe"
        );
        assert_eq!(escape_xml("<b>"), "&lt;b&gt;");
    }

    #[test]
    fn the_task_runs_windowless_and_without_a_time_limit() {
        let xml = task_xml(
            r"DOMAIN\user",
            r"C:\bloxsync.exe",
            "supervise --quiet",
            "S4U",
        );
        // S4U is what keeps it off the interactive desktop; without it a
        // console window appears at every logon.
        assert!(xml.contains("<LogonType>S4U</LogonType>"));
        // PT0S means no limit. The default would kill the supervisor after
        // three days and look like a random sync outage.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("IgnoreNew"), "never run two supervisors");
    }

    #[test]
    fn the_task_carries_the_command_it_was_given() {
        let xml = task_xml("u", r"C:\x\bloxsync.exe", "supervise --quiet", "S4U");
        assert_eq!(
            extract_tag(&xml, "Command").as_deref(),
            Some("C:\\x\\bloxsync.exe")
        );
        assert_eq!(
            extract_tag(&xml, "Arguments").as_deref(),
            Some("supervise --quiet")
        );
    }

    #[test]
    fn extract_tag_is_none_for_a_missing_tag() {
        assert_eq!(extract_tag("<a>1</a>", "b"), None);
    }
}
