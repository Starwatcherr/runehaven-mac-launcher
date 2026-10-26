//! Core of the unofficial Runehaven Launcher for macOS.
//!
//! * [`steam`]: finds the game in the user's Steam libraries, talks to the
//!   running Steam client and installs the Windows build through it.
//! * [`launch`]: starts the Steam-managed installation through Wine.
//! * [`doctor`]: system checks for setup and bug reports.

pub mod config;
pub mod doctor;
pub mod error;
pub mod launch;
pub mod maintain;
pub mod paths;
pub mod prefix;
pub mod runtime;
pub mod settings;
pub mod state;
pub mod steam;
mod util;
pub mod vdf;

pub use error::{Error, Result};
