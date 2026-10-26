use std::path::PathBuf;

/// All errors the core can report. Messages are written for the person in
/// front of the launcher, so they say what happened and, where possible, what
/// to do next.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Steam was not found. Expected the Steam app at {0}.")]
    SteamNotInstalled(PathBuf),

    #[error("Steam's console log was not found at {0}. Start Steam once and log in.")]
    ConsoleLogMissing(PathBuf),

    #[error("Steam did not become ready within {secs} seconds ({hint}).")]
    SteamNotReady { secs: u64, hint: String },

    #[error("Steam did not answer the command `{0}` in time. Its outcome is unknown; query the state again before retrying.")]
    NoResponse(String),

    #[error("Steam rejected the command `{command}`: {reason}")]
    CommandRejected { command: String, reason: String },

    #[error("Steam's console log was rotated while waiting for an answer. Query the state again.")]
    LogRotated,

    #[error("Invalid command token `{0}`. Only simple tokens without spaces, quotes or leading +/- are allowed.")]
    UnsafeToken(String),

    #[error("Could not read the platform setting from Steam's answer.")]
    PlatformUnreadable,

    #[error("Steam reports the platform as `{actual}` after setting it to `{expected}`.")]
    PlatformMismatch { expected: String, actual: String },

    #[error("Runehaven is not in this Steam account's library.")]
    NotOwned,

    #[error("Other Steam downloads are running ({0}). Wait until they have finished, because the platform switch would affect them too.")]
    OtherDownloadsActive(String),

    #[error("Steam did not start the installation within {0} seconds.")]
    InstallNotStarted(u64),

    #[error("Steam reported an error while installing (UpdateResult {0}).")]
    InstallFailed(u32),

    #[error("Steam could not check the files (Steam says: {0}). Try again in a moment.")]
    VerifyFailed(String),

    #[error("Steam could not install or update Runehaven (Steam says: {0}). Check your connection and try again.")]
    InstallFailedBecause(String),

    #[error("The installation made no progress for {0} seconds.")]
    InstallStalled(u64),

    #[error(
        "Cancelled. Steam's platform was restored; Steam may still finish the download on its own."
    )]
    Cancelled,

    #[error("Cancelled.")]
    Interrupted,

    #[error("Runehaven is not installed in any Steam library.")]
    GameNotInstalled,

    #[error("Steam is still updating Runehaven or an update is waiting. Let Steam finish first.")]
    GameNeedsUpdate,

    #[error("Runehaven is already running.")]
    AlreadyRunning,

    #[error("Could not parse {what}: {detail}")]
    Parse { what: String, detail: String },

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Configuration problem: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Small helper to attach a readable context to I/O errors.
pub(crate) trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context(),
            source,
        })
    }
}
