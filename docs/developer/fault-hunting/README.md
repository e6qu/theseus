# Developer exercise: Find and replay a retry bug

Use the service controls and guest markers to expose a
duplicate-apply bug. The sample models a two-node protocol inside one
bare-metal guest: a lost acknowledgement makes node A retry, and node B
applies the command again.

## 1. Build the guest

Use the Linux+KVM container from tutorial 1, then run:

```sh
apt-get install -y -qq binutils
rustup target add aarch64-unknown-none
sh /theseus/docs/developer/fault-hunting/guest/build.sh
```

## 2. Run the healthy schedule

```sh
cd /theseus/docs/developer/fault-hunting/driver && cargo run
```

The driver first sends two increments with no fault:

```text
clean schedule: [42, 01, 01, ff]
```

`0x01` means “applied once.”

## 3. Inject and replay the failure

The same driver sends `[0x05, 0xEE, 0x06]`. `0xEE` means “lose the last
acknowledgement.” The retry produces the failure marker:

```text
partition schedule: [42, 01, 02, 01, ff]   <- 0x02 is the duplicate apply
replay:             [42, 01, 02, 01, ff]
```

The second and third streams are identical. The fault schedule and seed
reproduce the bug.

## 4. Fix it

Edit `guest/main.rs`. Make the duplicate branch of `apply` ignore an already
seen command. Then update the partition assertion in `driver/src/main.rs`: it
should match the clean marker stream and contain no `0x02`. Rebuild the guest
and rerun the driver.

You now have the full loop: state a property as markers, inject a fault,
preserve the replay, and verify the patch.

See [determinism](../../determinism.md) and
[the control channel](../../control-channel.md).
