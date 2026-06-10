use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run a command with a hard timeout, optionally feeding it stdin.
/// Returns Err with a human-readable message on spawn failure or timeout.
pub fn output_timeout(
    mut cmd: Command,
    timeout: Duration,
    stdin: Option<&str>,
) -> Result<Output, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run {:?}: {e}", cmd.get_program()))?;

    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        let data = input.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
            // pipe drops here, closing stdin
        });
    }

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b);
        b
    });
    let err_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b);
        b
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("command timed out".into());
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(Output {
        status,
        stdout: out_h.join().unwrap_or_default(),
        stderr: err_h.join().unwrap_or_default(),
    })
}

/// First non-empty line of command stderr/stdout, trimmed — for error display.
pub fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error")
        .to_string()
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
