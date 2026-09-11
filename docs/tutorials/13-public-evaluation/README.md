# Tutorial 13: Read a public evaluation

Run every command from this directory. It contains one locked campaign result,
its minimization evidence, the evaluation contract, and its SHA-256 lock. It
needs only a published `theseus` binary on `PATH`.

```sh
theseus evaluate theseus-evaluation.toml
theseus evaluate --format markdown theseus-evaluation.toml > evaluation.md
```

The JSON result is stable enough for a dashboard. The Markdown result is a
short public evaluation note. Both distinguish replay-backed metrics from the
manually observed investigation duration and the conventional-chaos baseline.
Neither claims an equivalence with Antithesis.

The result says `Locked artifacts: verified` before it reports any campaign
metric. If you change this tutorial's campaign bundle, regenerate the tracked
lock before evaluating it:

```sh
theseus evaluate lock theseus-evaluation.toml
```

Run the complete check:

```sh
sh ./run.sh
```
