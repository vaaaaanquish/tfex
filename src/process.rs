//! Running external commands with a time limit.
//!
//! tfex leans on tools the user already has and has already signed in to
//! (`git`, `gcloud`, `aws`, `terraform`) instead of reimplementing their protocols and
//! credential handling. None of them may block a command indefinitely.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// Why a command produced no usable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The program could not be started, typically because it is not
    /// installed. Holds the reason given by the operating system.
    NotStarted(String),
    /// The command exited unsuccessfully. Holds what it wrote to standard
    /// error.
    Failed(String),
    /// The command was still running when its time was up.
    TimedOut,
}

/// What a command that ran to completion left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finished {
    /// Whether the command exited with status 0.
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Run `command` to completion and return what it wrote to standard output.
/// A command that exits unsuccessfully is a [`Failure::Failed`].
///
/// See [`run`] for how the command is run.
pub(crate) fn output(command: &mut Command, timeout: Duration) -> Result<Vec<u8>, Failure> {
    let finished = run(command, timeout)?;
    if finished.success {
        Ok(finished.stdout)
    } else {
        Err(Failure::Failed(
            String::from_utf8_lossy(&finished.stderr).into_owned(),
        ))
    }
}

/// Run `command` to completion, however it exits, and return what it wrote.
/// The only failures are [`Failure::NotStarted`] and [`Failure::TimedOut`].
///
/// The command gets no standard input. If it does not finish within
/// `timeout` it is killed. Processes it started itself may outlive it, but
/// are never waited for.
pub(crate) fn run(command: &mut Command, timeout: Duration) -> Result<Finished, Failure> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Failure::NotStarted(error.to_string()))?;

    // Both streams are drained on their own threads: that keeps a chatty
    // command from blocking on a full pipe, and lets the waits below time out.
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let streams = stdout
        .recv_timeout(timeout)
        .and_then(|output| Ok((output, stderr.recv_timeout(remaining(deadline))?)));
    let Ok((stdout, stderr)) = streams else {
        // Best effort: the process may already be gone.
        let _ = child.kill();
        let _ = child.wait();
        return Err(Failure::TimedOut);
    };

    match child.wait() {
        Ok(status) => Ok(Finished {
            success: status.success(),
            stdout,
            stderr,
        }),
        Err(error) => Err(Failure::NotStarted(error.to_string())),
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Read `stream` to its end on another thread; the result arrives on the
/// returned channel.
fn read_in_background<R>(stream: Option<R>) -> Receiver<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut contents = Vec::new();
        if let Some(mut stream) = stream {
            // A read error leaves what was read so far, which is still the
            // best account of what the command said.
            let _ = stream.read_to_end(&mut contents);
        }
        // The receiver is gone only if the command already timed out.
        let _ = sender.send(contents);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    const GENEROUS: Duration = Duration::from_secs(30);

    #[test]
    fn returns_standard_output_only() {
        let output = output(&mut shell("printf out; printf err >&2"), GENEROUS);
        assert_eq!(output, Ok(b"out".to_vec()));
    }

    #[test]
    fn reports_standard_error_of_a_failed_command() {
        let failure = output(
            &mut shell("printf out; printf 'no access' >&2; exit 3"),
            GENEROUS,
        );
        assert_eq!(failure, Err(Failure::Failed("no access".to_owned())));
    }

    #[test]
    fn run_keeps_the_output_of_a_failed_command() {
        let finished = run(&mut shell("printf out; printf err >&2; exit 3"), GENEROUS);
        assert_eq!(
            finished,
            Ok(Finished {
                success: false,
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
            })
        );
    }

    #[test]
    fn handles_output_larger_than_a_pipe_buffer() {
        let megabyte = 1 << 20;
        let script = format!("head -c {megabyte} /dev/zero; head -c {megabyte} /dev/zero >&2");
        let output = output(&mut shell(&script), GENEROUS).unwrap();
        assert_eq!(output.len(), megabyte);
    }

    #[test]
    fn reports_a_program_that_does_not_exist() {
        let failure = output(&mut Command::new("tfex-no-such-program"), GENEROUS);
        assert!(
            matches!(failure, Err(Failure::NotStarted(_))),
            "{failure:?}"
        );
    }

    #[test]
    fn kills_a_command_that_takes_too_long() {
        let started = Instant::now();
        let failure = output(&mut shell("exec sleep 30"), Duration::from_millis(200));
        assert_eq!(failure, Err(Failure::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
