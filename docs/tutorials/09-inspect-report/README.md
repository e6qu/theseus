# Tutorial 9: Inspect a recorded exploration

Run every command from this directory. This tutorial contains a small recorded
exploration, so it needs only the published `theseus` binary—no KVM host and no
guest build.

```sh
theseus report recorded-exploration
```

Open `recorded-exploration/theseus-report/index.html` in a browser. Read the
timeline tree in search order, then inspect the dirty-page footprint summary.

To attach the same evidence to an issue or CI job, write Markdown and JUnit
without running a VM:

```sh
theseus report --format markdown --output failure.md recorded-exploration
theseus report --format junit --output theseus-results.xml recorded-exploration
```

`failure.md` contains the exact locked replay command and seed paths. The
JUnit file has one test case per Theseus check. Keep the recorded exploration
directory with either file: the report explains the failure; the bundle is
what replays it.

To run the check used by this tutorial:

```sh
sh ./run.sh
```
