# Tutorial 13: Read a public evaluation

Run every command from this directory. It contains one locked campaign result,
its minimization evidence, and the evaluation contract. It needs only a
published `theseus` binary on `PATH`.

```sh
theseus evaluate theseus-evaluation.toml
theseus evaluate --format markdown theseus-evaluation.toml > evaluation.md
```

The JSON result is stable enough for a dashboard. The Markdown result is a
short public evaluation note. Both distinguish replay-backed metrics from the
manually observed investigation duration and the conventional-chaos baseline.
Neither claims an equivalence with Antithesis.

Run the complete check:

```sh
sh ./run.sh
```
