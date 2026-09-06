//! Direct child commands with bounded execution and process-group cleanup.
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    io::{self, Read, Write},
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
pub(super) enum CommandError {
    #[error("command start failed: {0}")]
    Start(io::Error),
    #[error("command timed out")]
    Timeout,
    #[error("interrupted")]
    Interrupted,
    #[error("command I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub(super) fn execute(
    argv: &[String],
    input: Option<Vec<u8>>,
    timeout: Duration,
    stopped: &AtomicBool,
) -> Result<Output, CommandError> {
    if stopped.load(Ordering::Relaxed) {
        return Err(CommandError::Interrupted);
    }
    let program = argv.first().ok_or_else(|| {
        CommandError::Start(io::Error::new(io::ErrorKind::InvalidInput, "empty command"))
    })?;
    let mut child = Command::new(program)
        .args(&argv[1..])
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CommandError::Start)?;
    let pid = Pid::from_raw(
        i32::try_from(child.id()).map_err(|error| io::Error::other(error.to_string()))?,
    );
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();
    let started = Instant::now();
    thread::scope(|scope| {
        let out = scope.spawn(move || read(stdout));
        let err = scope.spawn(move || read(stderr));
        let writer = scope.spawn(move || -> io::Result<()> {
            if let (Some(mut stream), Some(bytes)) = (stdin, input) {
                stream.write_all(&bytes)?;
            }
            Ok(())
        });
        let result = loop {
            if stopped.load(Ordering::Relaxed) {
                break Err(CommandError::Interrupted);
            }
            if started.elapsed() >= timeout {
                break Err(CommandError::Timeout);
            }
            match child.try_wait() {
                Ok(Some(status))
                    if out.is_finished() && err.is_finished() && writer.is_finished() =>
                {
                    break Ok(status);
                }
                Ok(_) => {}
                Err(error) => break Err(CommandError::Io(error)),
            }
            // Poll child status and SIGINT while output readers drain both pipes.
            thread::sleep(Duration::from_millis(10));
        };
        if result.is_err() {
            let _ = killpg(pid, Signal::SIGKILL);
            let _ = child.wait();
        }
        let stdout = out
            .join()
            .map_err(|_| io::Error::other("stdout reader panicked"))?;
        let stderr = err
            .join()
            .map_err(|_| io::Error::other("stderr reader panicked"))?;
        let written = writer
            .join()
            .map_err(|_| io::Error::other("stdin writer panicked"))?;
        let status = result?;
        written?;
        Ok(Output {
            status,
            stdout: stdout?,
            stderr: stderr?,
        })
    })
}

fn read(stream: Option<impl Read>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .ok_or_else(|| io::Error::other("command output pipe unavailable"))?
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}
