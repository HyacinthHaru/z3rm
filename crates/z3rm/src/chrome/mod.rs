//! Native chrome baseline (spec §5.1).
//!
//! GPUI views that serve as Day 0 baseline chrome. When a QuickJS chrome
//! extension activates, it replaces these views via the VDOM bridge.

pub mod status_bar;
pub mod tab_bar;

pub use status_bar::StatusBar;
pub use tab_bar::TabBar;
