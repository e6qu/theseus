# Tutorial 2: Script guest randomness

Use the service API to choose the bytes an unchanged C guest reads. Complete
tutorial 1 first; reuse its container and built `firecracker` binary.

## 1. Run the scripted guest

```sh
sh /theseus/docs/tutorials/02-control-the-random/run.sh
```

The guest is [init.c](init.c). It opens `/dev/hwrng`, reads four `u64` values,
prints them, and exits. It does not use `theseus-sdk`.

```text
random() = 1 2 3 4
PASS: random() returned 1, 2, 3, 4 — the values we scripted
```

## 2. See the control point

The script starts the same service as tutorial 1, then sends this request:

```sh
curl --unix-socket "$SOCK" -X PUT localhost/entropy \
  -H 'Content-Type: application/json' \
  -d "{\"seed\": 42, \"script\": [$SCRIPT]}"
```

`script` is served before the seeded stream. Its bytes encode the four
little-endian values `1`, `2`, `3`, and `4`. The helper script also accounts
for the bytes Linux consumes while it initializes its entropy pool.

Use this to force a randomized branch, retry count, or timeout choice. Next,
make the guest report whether the chosen input produced the result you want.

See [the entropy model](../../determinism.md).
