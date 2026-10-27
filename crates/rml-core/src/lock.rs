//! One task at a time. Starting the game, setting up, maintenance and the
//! jobs that switch Steam's platform all change the same things (the Wine
//! prefix, the game folder, Steam's settings, `state.json`), so each holds
//! this lock while it runs.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::error::{Error, IoContext, Result};
use crate::paths::LauncherPaths;

pub(crate) const LOCK_FILE: &str = "launcher.lock";
pub(crate) const PID_FILE: &str = "run.pid";

/// Held by the running task; dropping it lets the next one start. It is an
/// `flock` on `launcher.lock`, so it also holds between the app and the
/// command line tool, and a crashed process cannot leave it behind. A game
/// start holds it while it prepares; while the game runs, its process id is
/// in `run.pid`, which keeps other tasks out until it has exited.
pub struct JobLock {
    _file: File,
    pid_file: PathBuf,
}

impl JobLock {
    /// Takes the lock, or fails at once with [`Error::Busy`] while another
    /// task holds it and with [`Error::AlreadyRunning`] while the game runs.
    pub fn acquire(launcher: &LauncherPaths) -> Result<Self> {
        let lock = Self::acquire_beside_game(launcher)?;
        if game_running(&lock.pid_file) {
            return Err(Error::AlreadyRunning);
        }
        Ok(lock)
    }

    /// Like [`Self::acquire`], but also while the game runs: for tasks that
    /// only talk to Steam, such as switching its platform back.
    pub fn acquire_beside_game(launcher: &LauncherPaths) -> Result<Self> {
        std::fs::create_dir_all(&launcher.root)
            .ctx(|| format!("creating {}", launcher.root.display()))?;
        let path = launcher.root.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .ctx(|| format!("opening {}", path.display()))?;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(Error::Busy);
            }
            return Err(err).ctx(|| format!("locking {}", path.display()));
        }
        Ok(Self { _file: file, pid_file: launcher.root.join(PID_FILE) })
    }

    /// Notes the started game, so that no task starts while it runs, also
    /// after this lock is gone.
    pub fn record_game(&self, pid: u32) -> Result<()> {
        std::fs::write(&self.pid_file, pid.to_string())
            .ctx(|| format!("writing {}", self.pid_file.display()))
    }
}

/// The game the launcher started has exited.
pub fn game_exited(launcher: &LauncherPaths) {
    let _ = std::fs::remove_file(launcher.root.join(PID_FILE));
}

/// Whether the game a launcher started is still running.
fn game_running(pid_file: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<libc::pid_t>() else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_task_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let first = JobLock::acquire(&launcher).unwrap();
        assert!(matches!(JobLock::acquire(&launcher), Err(Error::Busy)));
        drop(first);
        let second = JobLock::acquire(&launcher).unwrap();

        // A running game blocks every task until it has exited, except
        // those that only talk to Steam.
        second.record_game(std::process::id()).unwrap();
        drop(second);
        assert!(matches!(JobLock::acquire(&launcher), Err(Error::AlreadyRunning)));
        drop(JobLock::acquire_beside_game(&launcher).unwrap());
        std::fs::write(launcher.root.join(PID_FILE), "999999999").unwrap();
        drop(JobLock::acquire(&launcher).unwrap());
        game_exited(&launcher);
        assert!(!launcher.root.join(PID_FILE).exists());
    }
}
