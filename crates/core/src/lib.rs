//! Pure core: no I/O; time comes in as a [`Stamp`]. Enforced by `clippy.toml`.

pub mod monitor;
pub mod parse;
pub mod ring;
pub mod stamp;
pub mod tape;

pub use ring::Ring;
pub use stamp::Stamp;
