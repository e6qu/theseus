# Theseus CLI

`theseus` is the local entry point for a self-contained Theseus test
directory. It validates the test contract, runs one Firecracker timeline, and
preserves a self-contained replay bundle.

## Commands

```sh
theseus validate [theseus.toml]
theseus test --dry-run [theseus.toml]
theseus test [--output replay-dir] [theseus.toml]
theseus replay replay-dir
```

`validate` checks the manifest and artifacts. `test --dry-run` prints the
normalized plan, including SHA-256 digests. It does not need KVM and does not
start a VM.

`test` runs one Linux+KVM Firecracker timeline. It copies the runtime binary,
kernel, and initramfs into an immutable replay directory before booting, then
writes the resolved source plan, bundle-local replay plan, serial log,
Firecracker log, and result there. The default output is
`theseus-replay/` beside the manifest; pass `--output` to choose another empty
directory. `replay` uses only the copied artifacts and leaves its source bundle
unchanged.

The CLI is released for Linux amd64/arm64 and macOS arm64. macOS supports
validation and planning only: a Firecracker timeline needs Linux and KVM.

## Test directory

Keep the manifest, extracted published Theseus runtime bundle, kernel, and
initramfs in one directory. Paths in the manifest are relative to that
directory; paths that escape it are rejected.

```text
my-test/
├── theseus.toml
├── runtime/firecracker
└── guest/
    ├── vmlinux
    └── initramfs.cpio.gz
```

Use the Firecracker binary from an extracted, SHA-addressed Theseus release
bundle. Do not point the manifest at a source checkout.

```toml
version = 1

[runtime]
firecracker = "runtime/firecracker"

[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio.gz"

[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
timeout_secs = 30

[run.virtual_time]
tick_ns = 1000000
exits_per_tick = 1024

[[events]]
when = "ready"
data = "0100ff"

[network]
loopback = true
drop_ppm = 0
partitioned = false
```

`events.data` is an even-length hexadecimal byte string. Version 1 has one
delivery point: `ready`, after the guest announces that it can receive input.
The future executor will preserve the resulting plan verbatim in its replay
bundle. P6.2 delivers serial bytes only after the `THES:M:42` ready marker.
Network drop and partition settings are recorded but intentionally rejected by
this single-VM runner; P6.4 will add a deterministic topology to apply them.
