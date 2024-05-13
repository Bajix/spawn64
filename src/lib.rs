#![cfg_attr(feature = "strict_provenance", feature(strict_provenance))]

#[cfg(any(target_arch = "wasm32", doc))]
mod arena;
#[cfg(any(target_arch = "wasm32", doc))]
mod handle;
#[cfg(any(target_arch = "wasm32", doc))]
mod runtime;

#[cfg(any(target_arch = "wasm32", doc))]
pub use runtime::spawn_local;
