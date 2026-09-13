# Tutorial 10: Find and minimize a bad timeline

Drive three tiny Linux services with named UART operations. Theseus explores
bounded operation and fault schedules until the intentionally broken workload
reports a stale read, then minimizes and replays that counterexample.

Use Linux on arm64 with KVM and Docker. Run every host command from this
directory:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside the container.

## 1. Inspect the campaign

```sh
sed -n '1,320p' compose.yaml
```

Read the `campaign` section in this order: stages and operations, fault
candidates, then properties. Operations carry stable names and explicit UART
input. The guests print operation checkpoints and JSON evidence. The campaign
uses those retained observations, not host wall time, to extend a timeline.

## 2. Build the three guest inputs

```sh
for service in api replica auditor; do
  mkdir -p "$service/runtime" "$service/guest/root/bin"
  cp /usr/local/bin/firecracker "$service/runtime/firecracker"
  cp /opt/theseus/vmlinux "$service/guest/vmlinux"
  cp /bin/busybox "$service/guest/root/bin/busybox"
  for applet in mount sleep poweroff; do
    ln -sf busybox "$service/guest/root/bin/$applet"
  done
  cp "$service/init" "$service/guest/root/init"
  chmod +x "$service/guest/root/init"
  (cd "$service/guest/root" && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
done
```

## 3. Find the intentional failure

```sh
if theseus compose explore --output theseus-compose-campaign; then
  echo 'expected the intentional stale-read property failure' >&2
  exit 1
fi
grep -a '"name": "consistent_read"' theseus-compose-campaign/campaign-result.json
grep -a '"status": "failed"' theseus-compose-campaign/campaign-result.json
grep -a '"checkpoint": {' theseus-compose-campaign/campaign-result.json
grep -a '"faults": \[' theseus-compose-campaign/campaign-result.json
```

The command is expected to fail because it found the tutorial's planted bug.
The result retains the operation boundary, property verdict, and applied fault
schedule. It does not by itself establish application basic-block coverage or
counterfactual causality.

## 4. Minimize and replay

```sh
if theseus compose explore --minimize theseus-compose-campaign --output stale-read-replay; then
  echo 'expected the minimized stale-read counterexample' >&2
  exit 1
fi
grep -a '"property": "consistent_read"' stale-read-replay/minimization.json
grep -a '"operation_attempts":' stale-read-replay/minimization.json
grep -a '"fault_attempts":' stale-read-replay/minimization.json
theseus compose replay stale-read-replay --output stale-read-rerun
grep -a 'counterexample: consistent_read' stale-read-rerun/services/api/result.json
```

## 5. Render the retained evidence

```sh
theseus report --output campaign-report theseus-compose-campaign
theseus report --output minimized-report stale-read-replay
test -f campaign-report/index.html
test -f minimized-report/index.html
exit
```

## 6. Clean up (optional)

```sh
rm -rf api/runtime api/guest replica/runtime replica/guest \
  auditor/runtime auditor/guest theseus-compose-campaign stale-read-replay \
  stale-read-rerun campaign-report minimized-report
```
