//! GTK widget tree. Everything here runs on the main thread.

pub mod dock;
pub mod geometry;
pub mod menu;
pub mod settings;

pub use dock::DockSurface;
pub use geometry::Geometry;
