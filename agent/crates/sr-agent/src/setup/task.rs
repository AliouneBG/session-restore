//! The logon task that actually starts the agent after a reboot.
//!
//! A Scheduled Task, not a Windows Service, and not a Run key:
//!
//! - **Not a service**, because Session 0 isolation would make the agent unable to see
//!   the user's windows, place them, or use the user's DPAPI key (ADR-0001).
//! - **Not a Run key**, because a Run entry cannot express "restart if it dies",
//!   "delay 30 seconds", or "keep running on battery", and it is trivially clobbered.
//!
//! Registered per-user under the current account, so no admin rights are needed.

use anyhow::{Context, Result};
use std::path::Path;

pub const TASK_NAME: &str = "SessionRestore\\Agent";

/// Builds the task definition.
///
/// Two settings are load-bearing and deliberately contrary to what a template would
/// give you:
///
/// - `RunLevel = LeastPrivilege`. The agent never needs admin, and an always-running
///   elevated process that launches other processes from a writable database would be
///   a standing privilege-escalation primitive.
/// - `DisallowStartIfOnBatteries = false`. The default is true, which would silently
///   mean "your session is not captured on a laptop unless it is plugged in" - i.e.
///   broken for exactly the machines most likely to be rebooted away from a desk.
fn task_xml(exe: &Path, user: &str) -> String {
    let exe = exe.display().to_string();
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Captures your open applications and browser tabs so they can be restored after a restart.</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT30S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
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
    <Hidden>false</Hidden>
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
      <Command>"{exe}"</Command>
      <Arguments>--logon</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();
    if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    }
}

/// Registers (or replaces) the logon task.
#[cfg(windows)]
pub fn install(exe: &Path) -> Result<()> {
    use std::io::Write;

    let xml = task_xml(exe, &current_user());

    // schtasks requires UTF-16LE with a BOM when importing XML, which is easy to get
    // wrong and fails with an unhelpful "The task XML is malformed".
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    let path = std::env::temp_dir().join(format!("sr-task-{}.xml", sr_proto::new_id()));
    {
        let mut f = std::fs::File::create(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(&bytes)?;
    }

    let out = std::process::Command::new("schtasks.exe")
        .args([
            "/Create",
            "/TN",
            TASK_NAME,
            "/XML",
            &path.display().to_string(),
            "/F", // replace an existing definition
        ])
        .output()
        .context("running schtasks /Create")?;

    let _ = std::fs::remove_file(&path);

    if !out.status.success() {
        anyhow::bail!(
            "schtasks /Create failed: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> Result<()> {
    let out = std::process::Command::new("schtasks.exe")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()
        .context("running schtasks /Delete")?;
    // Absent is the desired end state, so "not found" is success.
    if !out.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if !text.contains("cannot find") && !text.contains("does not exist") {
            anyhow::bail!("schtasks /Delete failed: {}", text.trim());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn is_installed() -> bool {
    std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn install(_exe: &Path) -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
pub fn uninstall() -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
pub fn is_installed() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_never_asks_for_admin() {
        // The single most important line in the definition.
        let xml = task_xml(Path::new(r"C:\x\sr-agent.exe"), "DOMAIN\\user");
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(!xml.contains("HighestAvailable"));
    }

    #[test]
    fn it_runs_on_battery() {
        // The Task Scheduler default is to refuse, which would mean "not captured on a
        // laptop unless plugged in" - broken for the machines most likely to reboot.
        let xml = task_xml(Path::new(r"C:\x\sr-agent.exe"), "u");
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
    }

    #[test]
    fn it_has_no_execution_time_limit() {
        // The agent runs for the whole session; the default 72h limit would kill it.
        let xml = task_xml(Path::new(r"C:\x\sr-agent.exe"), "u");
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn it_waits_for_the_desktop_to_settle() {
        let xml = task_xml(Path::new(r"C:\x\sr-agent.exe"), "u");
        assert!(xml.contains("<Delay>PT30S</Delay>"));
    }

    #[test]
    fn a_second_logon_does_not_start_a_second_agent() {
        let xml = task_xml(Path::new(r"C:\x\sr-agent.exe"), "u");
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }

    #[test]
    fn the_exe_path_is_quoted() {
        // Program Files has a space in it; an unquoted path silently runs the wrong
        // thing or nothing at all.
        let xml = task_xml(Path::new(r"C:\Program Files\App\sr-agent.exe"), "u");
        assert!(xml.contains(r#"<Command>"C:\Program Files\App\sr-agent.exe"</Command>"#));
    }

    #[test]
    fn it_restarts_if_it_dies() {
        let xml = task_xml(Path::new(r"C:\x\sr-agent.exe"), "u");
        assert!(xml.contains("<RestartOnFailure>"));
    }
}
