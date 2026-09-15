//! Running external commands with a hard deadline.
//!
//! Every call the daemon makes to herdr, `lsof` or `ps` goes through
//! [`run_with_timeout`], so a wedged tool can never stall the scan loop.

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Run `command`, kill it if it exceeds `timeout`, and return its output.
///
/// Stdout and stderr are drained on helper threads so a chatty child cannot
/// deadlock on a full pipe. A timed-out child is killed and reaped.
pub fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Output> {
    let display = describe(&command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {display}"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = match wait_with_deadline(&mut child, timeout) {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{display} timed out after {} ms", timeout.as_millis());
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Like [`run_with_timeout`] but fails on a non-zero exit and returns stdout as UTF-8.
pub fn run_checked(command: Command, timeout: Duration) -> Result<String> {
    let display = describe(&command);
    let output = run_with_timeout(command, timeout)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{display} exited with {}: {}", output.status, stderr.trim());
    }
    String::from_utf8(output.stdout).with_context(|| format!("{display}: stdout is not UTF-8"))
}

fn wait_with_deadline(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    // Poll with try_wait so we do not need an OS-specific waitpid-with-timeout.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn drain<R: Read + Send + 'static>(reader: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut reader) = reader {
            let _ = reader.read_to_end(&mut buf);
        }
        buf
    })
}

pub fn describe(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(command.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}
