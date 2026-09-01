# Tutorial 9: Inspect a recorded exploration

Run every command from this directory. This tutorial contains a small recorded
exploration, so it needs only the published `theseus` binary—no KVM host and no
guest build.

```sh
theseus report recorded-exploration
```

Open `recorded-exploration/theseus-report/index.html` in a browser. Read the
timeline tree in search order, then inspect the dirty-page footprint summary.

To run the check used by this tutorial:

```sh
sh ./run.sh
```
