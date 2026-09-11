# Tutorial 12: Investigate two campaign bundles

Run every command from this directory. It contains two completed campaign
results, so it needs only a published `theseus` binary on `PATH`. No VM,
source checkout, or snapshot is needed.

```sh
theseus compare campaign-before campaign-after
```

The result names the first changed operation boundary. In these fixtures, the
same `read` operation first reaches a different topology state after a network
partition. The bundle retains the fault action, serial digest, and coverage
evidence, so inspect the cause without diffing a serial log or opening a VM
snapshot.

Ask for a compact issue-ready report:

```sh
theseus compare --format markdown campaign-before campaign-after > investigation.md
```

Inspect any retained field with an RFC 6901 JSON Pointer. This reads the paused
PC samples at the divergent boundary:

```sh
theseus compare --query /runs/0/timeline/1/program_counters \
  campaign-before campaign-after
```

Pointers also reach fault actions, serial evidence, topology hashes, and
property verdicts. For example:

```sh
theseus compare --query /properties/0/status campaign-before campaign-after
```

Run the complete check:

```sh
sh ./run.sh
```
