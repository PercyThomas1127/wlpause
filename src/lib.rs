//! wlpause -- pause video wallpapers while they are covered.
//!
//! The compositor is the only thing that knows whether a background
//! layer-shell surface is actually visible; no Wayland protocol exposes that
//! to the client. So each backend queries its compositor's IPC, reduces the
//! answer to rectangles, and the shared logic here decides whether to pause.

pub mod compositor;
pub mod coverage;
pub mod decide;
pub mod target;
