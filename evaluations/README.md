# Public evaluations

Each directory is a versioned, offline-readable evaluation. Its
`theseus-evaluation.toml` names only bundles below that directory, states the
expected property outcome, and records a conventional baseline separately from
Theseus evidence. Run it with a published `theseus` binary:

```sh
theseus evaluate --format markdown replicated-counter/theseus-evaluation.toml
```

The machine report counts replay verification, generated candidates, retained
runs, topology and instruction-location coverage, checkpoint work, reduction
work, and retained operation boundaries. A manually recorded investigation
duration is informational only: no host-time value changes an evaluation
verdict or replay claim.

The conventional baseline is a declared observation, not a claim of
equivalence with Antithesis or any other product. Keep its method, run count,
and counterexample count in the suite so readers can judge the comparison.
