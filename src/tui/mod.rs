//NetEase Cloud Music terminal frontend.

mod app;
mod draw;
mod input;
mod layout;
mod types;

#[cfg(test)]
mod tests;

pub use app::run;
pub use types::{Services, TuiError};

pub(in crate::tui) use app::*;
pub(in crate::tui) use draw::*;
pub(in crate::tui) use input::*;
pub(in crate::tui) use layout::*;
pub(in crate::tui) use types::*;
