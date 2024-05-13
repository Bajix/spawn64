#![cfg(any(target_arch = "wasm32", doc))]
#![cfg_attr(feature = "strict_provenance", feature(strict_provenance))]

mod arena;
mod handle;
mod runtime;

pub use runtime::spawn_local;
