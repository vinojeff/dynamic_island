//! Pure decisions for the Dynamic Edge overlay.
//!
//! The binary owns the window and every thread. This library never opens one.
//! A caller hands in numbers (pointer, timestamps, a layer sample) and gets
//! back the next shape, the next page, or whether the dock should hide.
//!
//! Read [`island`] for geometry, then [`notice`], [`volume`], [`media`],
//! [`perf`], and [`clock`] for the content those shapes display. The view in
//! the binary is the only module that applies the answers.

#![warn(missing_docs)]

pub mod clock;
pub mod island;
pub mod media;
pub mod notice;
pub mod perf;
pub mod volume;
