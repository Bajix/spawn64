# Spawn64
![License](https://img.shields.io/badge/license-MIT-green.svg)
[![Cargo](https://img.shields.io/crates/v/spawn64.svg)](https://crates.io/crates/spawn64)
[![Documentation](https://docs.rs/spawn64/badge.svg)](https://docs.rs/spawn64)

Spawn64 provides an optimized alternative to `wasm-bindgen-futures::spawn_local` capable of handling at most 64 concurrent futures and without sending to JavaScript.