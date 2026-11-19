//! Where Prostir's monitors go, as pure functions both sides share: no dependencies, no
//! input or output, so everything here is tested on the PC.
//!
//! - [`desk`]: the PC desktop, in logical pixels, from the rows the headset shows the
//!   monitors in. The PC applies it; the headset draws its map with the same function.
//!
//! Every function is generic over the key that names a monitor (`K: Copy + Ord`), so
//! the numbers each side uses can change without changing the rules.

pub mod desk;
