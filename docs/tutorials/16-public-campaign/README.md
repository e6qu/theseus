# Tutorial 16: Publish a public campaign

Run every command from this directory. You need Linux, KVM, and Docker. Set a
published Theseus runtime image:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

The tutorial runs a normal BusyBox HTTP image as a Compose campaign, then
publishes its completed replay directory:

```sh
theseus evaluate capture campaign --output public-evaluation --name "HTTP health campaign"
```

`public-evaluation/` contains the copied campaign, a version 2 evaluation
contract, and a lock covering every copied file. Give that directory to a
reader with the published `theseus` binary. They can inspect the result and
replay the campaign without this source tree or the original campaign output.

Use capture only after the campaign is complete. It records the observed
status and property outcomes; add an independently repeatable conventional
baseline later if you want one.
