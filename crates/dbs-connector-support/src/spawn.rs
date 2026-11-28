//! Starting a child process, retrying the transient "text file busy" error.
//!
//! On Unix, `exec` fails with `ETXTBSY` if any process still has the
//! executable open for writing. That happens when a program is run right
//! after it was written: another thread may `fork` while the file is open,
//! and the forked child holds that descriptor until it `exec`s. It is
//! common in tests that write a fake `yt-dlp` script and run it at once
//! (it made a CI test fail intermittently), and it can also follow a
//! `yt-dlp` self-update. The window is short, so a few brief retries
//! clear it; every other spawn error is returned immediately.

use std::io;
use std::process::{Child, Command};
use std::time::Duration;

/// How many extra attempts to make after a "text file busy" spawn error.
const BUSY_RETRIES: u32 = 20;
/// Pause between attempts. 20 x 50ms bounds the total wait at about 1s.
const BUSY_PAUSE: Duration = Duration::from_millis(50);

/// [`Command::spawn`], retried briefly while the executable is busy.
pub fn spawn_retrying_busy(cmd: &mut Command) -> io::Result<Child> {
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Err(e) if is_text_file_busy(&e) && attempt < BUSY_RETRIES => {
                attempt += 1;
                std::thread::sleep(BUSY_PAUSE);
            }
            result => return result,
        }
    }
}

/// Whether `e` is `ETXTBSY` (errno 26 on Linux and macOS).
fn is_text_file_busy(e: &io::Error) -> bool {
    const ETXTBSY: i32 = 26;
    cfg!(unix) && e.raw_os_error() == Some(ETXTBSY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runnable_program_spawns_normally() {
        // `rustc` is always present where these tests build.
        let mut cmd = Command::new("rustc");
        cmd.arg("--version").stdout(std::process::Stdio::null());
        let mut child = spawn_retrying_busy(&mut cmd).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn other_spawn_errors_are_returned_without_retrying() {
        let started = std::time::Instant::now();
        let err = spawn_retrying_busy(&mut Command::new("definitely-not-a-real-program-xyz"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            started.elapsed() < BUSY_PAUSE,
            "a missing program must fail at once, not wait out the retries"
        );
    }

    #[test]
    fn only_etxtbsy_counts_as_busy() {
        assert_eq!(
            is_text_file_busy(&io::Error::from_raw_os_error(26)),
            cfg!(unix)
        );
        assert!(!is_text_file_busy(&io::Error::from_raw_os_error(2)));
        assert!(!is_text_file_busy(&io::Error::other("busy")));
    }
}
