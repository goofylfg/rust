#![crate_name = "rand"]
#![deny(intra_doc_resolution_failure)]

pub trait RngCore {}
/// Rng extends [`RngCore`].
pub trait Rng: RngCore {}
