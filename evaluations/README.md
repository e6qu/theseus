# Public evaluations

Each directory is a versioned, offline-readable evaluation. Its
`theseus-evaluation.toml` names only bundles below that directory, states the
expected property outcome, and records a conventional baseline separately from
Theseus evidence. Version 2 contracts also name an artifact lock. Evaluation
verifies the SHA-256 and size of every regular file in every referenced bundle
before it reads the campaign result. Run it with a published `theseus` binary:

```sh
theseus evaluate --format markdown replicated-counter/theseus-evaluation.toml
```

When changing a version 2 corpus, regenerate and commit its lock after the
bundle is complete:

```sh
theseus evaluate lock replicated-counter/theseus-evaluation.toml
```

The lock rejects changed, missing, unexpected, and symlinked bundle files. It
is evidence integrity, not a signature or a claim that a host-time baseline is
reproducible.

Publish a completed KVM campaign without hand-copying its replay artifacts:

```sh
theseus evaluate capture campaign-dir --output public-evaluation --name "service failure"
```

Capture copies the complete replay directory, derives expected property
outcomes from its recorded result, writes a version 2 contract, and locks every
copied file. Add the conventional baseline only after its observation is
independently reproducible.

The machine report counts replay verification, generated candidates, retained
runs, topology and instruction-location coverage, checkpoint work, reduction
work, and retained operation boundaries. A manually recorded investigation
duration is informational only: no host-time value changes an evaluation
verdict or replay claim.

The conventional baseline is a declared observation, not a claim of
equivalence with Antithesis or any other product. Keep its method, run count,
and counterexample count in the suite so readers can judge the comparison.
