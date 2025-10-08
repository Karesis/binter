# binter

[![Crates.io](https://img.shields.io/crates/v/binter.svg)](https://crates.io/crates/binter)
[![Docs.rs](https://docs.rs/binter/badge.svg)](https://docs.rs/binter)
[![License](https://img.shields.io/crates/l/binter.svg)](https://github.com/Karesis/binter/blob/main/LICENSE-MIT)
[![Build Status](https://github.com/Karesis/binter/actions/workflows/rust.yml/badge.svg)](https://github.com/Karesis/binter/actions)

A high-performance, thread-safe string interner backed by a concurrent bump allocator.

`binter` is designed to provide extremely fast string-to-symbol interning in a multi-threaded environment. It is especially useful for applications that frequently compare strings, such as compilers, databases, or game engines, by converting expensive string comparisons into trivial integer comparisons.

---

## ✨ Features

* **High Performance & Concurrency**: The read path (looking up a symbol) uses an `RwLock` for concurrent access, while the write path's memory allocation is handled by a concurrency-optimized `SyncBump` allocator with a lock-free fast path.
* **Blazingly Fast Allocation**: Backed by a bump allocator, allocating new strings is typically as fast as a pointer bump.
* **Thread-Safe by Design**: Intended to be used as a global static instance, safely shared across all threads.
* **Lightweight Symbols**: The `Symbol` type is a `Copy`-able wrapper around a `u32`, making it cheap to pass, store, and use as a `HashMap` key.

## 🚀 Quick Start

Add `binter` to your `Cargo.toml`:

```bash
cargo add binter
```

Or add it manually:

```toml
[dependencies]
binter = "0.1.0"
once_cell = "1.0" # Recommended for creating the global instance
```

### Usage Example

`binter` is designed to be used as a **global static instance**. The `once_cell` crate is the idiomatic way to create such an instance.

```rust
use binter::Interner;
use once_cell::sync::Lazy;

// 1. Create a global, thread-safe Interner instance.
static GLOBAL_INTERNER: Lazy<Interner<'static>> = Lazy::new(|| Interner::with_capacity(1024));

fn main() {
    // 2. Intern strings from different places (or threads).
    let sym_hello = GLOBAL_INTERNER.intern("hello");
    let sym_world = GLOBAL_INTERNER.intern("world");
    let sym_hello_again = GLOBAL_INTERNER.intern("hello");

    // 3. The same string yields the same Symbol.
    assert_eq!(sym_hello, sym_hello_again);
    assert_ne!(sym_hello, sym_world);

    // 4. You can quickly resolve the original string from a Symbol.
    let resolved_str = GLOBAL_INTERNER.resolve(sym_world);
    assert_eq!(resolved_str, Some("world"));

    println!("Symbol for 'hello': {:?}", sym_hello);
    println!("'world' string is: {}", resolved_str.unwrap());
}
```

## 📜 Project Status & Background

* **Origin**: `binter` was extracted from [Nyan](https://github.com/Karesis/nyan), a personal compiler project for a custom programming language. Its design and implementation have been validated in a real-world compiler setting.
* **Current Stage**: This is an early-stage release (`0.1.x`). While its core functionality is stable and performant within the context of the Nyan compiler, it has not yet been extensively benchmarked or tested in other application domains (e.g., web services, gaming).
* **Testing**: The current test suite is minimal. Contributions in the form of new test cases, usage scenarios, and performance benchmarks are highly encouraged!

## License

This project is dual-licensed under your choice of the following:

* **MIT License** ([`LICENSE-MIT`](LICENSE-MIT) or http://opensource.org/licenses/MIT)
* **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

## Contributing

Contributions of all kinds are welcome! Feel free to open an issue, submit a pull request for a bug fix, add more tests, or improve the documentation.

## Acknowledgements

The design and implementation of the underlying `SyncBump` allocator are heavily inspired by the excellent [`bumpalo`](https://github.com/rust-lang/bumpalo) crate. Thank you to its authors for their great work for the Rust community.