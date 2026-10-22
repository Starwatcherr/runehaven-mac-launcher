//! Everything that touches the user's Steam installation.

pub mod client;
pub mod console;
pub mod install;
pub mod library;

pub use client::{MacTransport, ReadyStatus, SteamClient, Timing, Transport};
pub use install::{
    install, recover_platform, InstallEvent, InstallOptions, Installed, RestoreAt, RUNEHAVEN_APPID,
};
pub use library::{AppManifest, Library};
