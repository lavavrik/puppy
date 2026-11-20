//! puppy's full-screen terminal dashboard.
//!
//! Rendering code is pure (it draws into a ratatui buffer from the app state); terminal
//! I/O lives only in the runtime.

pub mod api;
pub mod app;
pub mod format;
pub mod runtime;
pub mod theme;
pub mod ui;
pub mod widgets;
