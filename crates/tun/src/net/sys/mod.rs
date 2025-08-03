#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::*;
