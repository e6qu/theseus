# Tutorial 3: Mark guest state with `theseus-sdk`

Build a small bare-metal guest that detects the Theseus control device, emits
setup and boot markers, echoes input events, and signals completion.

Run every command from this directory. You need Rust, `rustup`, `objcopy`, and
a published 12-character Theseus release SHA. The SDK is downloaded from that
release; no Theseus checkout is used.

```sh
export THESEUS_TAG=<12-character-sha>
```

## 1. Inspect the guest

```sh
sed -n '1,240p' main.rs
sed -n '1,120p' Cargo.toml
```

The marker values form the guest/service contract. Give them stable meanings,
such as “ready” or “invariant failed.”

## 2. Download the published SDK

```sh
rm -rf vendor target guest.bin
mkdir vendor
curl -fsSL \
  "https://github.com/e6qu/theseus/releases/download/$THESEUS_TAG/theseus-sdk-0.1.0.crate" \
  | tar -xz -C vendor --strip-components=1
```

`Cargo.toml` points only at the new local `vendor` directory.

## 3. Build the guest

```sh
rustup target add aarch64-unknown-none
cargo build --release
objcopy -O binary \
  target/aarch64-unknown-none/release/theseus-sdk-tutorial guest.bin
test -s guest.bin
```

`guest.bin` is the resulting aarch64 guest image.

## 4. Clean up (optional)

```sh
rm -rf vendor target guest.bin
```
