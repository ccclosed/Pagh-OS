# pagh mini-Rust

`pagh-mini` is a small offline Rust-like interpreter built into the kernel shell. It exists so files written with `nano` can run immediately; it is not upstream rustc.

## Commands

```text
cargo new /mnt/demo
cargo check /mnt/demo
cargo build /mnt/demo
cargo run /mnt/demo
rustc /mnt/demo/src/main.rs -o /mnt/demo.pbc
rust /mnt/demo.pbc
rustup show
```

## Language subset

```rust
fn main() {
    let numbers = [10, 20, 30, 40];
    let sum: i32 = numbers.iter().sum();
    println!("sum = {}", sum);

    for i in 1..=5 {
        println!("iteration {}", i);
    }
}
```

Supported: signed 64-bit integer arithmetic, variables, integer arrays, `iter().sum()`, assignments, `print!`, `println!`, and integer `for` ranges. Source and packages are limited to 64 KiB.

Not supported: crates/dependencies, generics, structs, enums, traits, borrowing, async, macros other than print, native code generation, or network toolchain downloads. Use the host musl workflow for full Rust.
