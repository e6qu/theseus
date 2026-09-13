# Tutorial 13: Read a public evaluation

Render JSON and Markdown summaries from the evaluation fixture in this
directory. The fixture demonstrates the offline format; it does not contain a
complete independently replayable campaign.

Run every command from this directory with a published `theseus` binary on
`PATH`. No KVM, Docker, or source checkout is needed.

## 1. Inspect the declared inputs

```sh
sed -n '1,220p' theseus-evaluation.toml
sed -n '1,120p' theseus-evaluation.lock
```

The lock verifies the retained fixture bytes. A valid hash does not prove that
the declared execution happened.

## 2. Render both formats

```sh
if theseus evaluate theseus-evaluation.toml > rendered-evaluation.json; then
  echo 'expected the incomplete format fixture to fail evaluation' >&2
  exit 1
fi
grep -F 'theseus-public-evaluation-v1' rendered-evaluation.json
grep -F '"status": "failed"' rendered-evaluation.json
grep -F '"verified": 0' rendered-evaluation.json
if theseus evaluate --format markdown theseus-evaluation.toml > rendered-evaluation.md; then
  echo 'expected the incomplete format fixture to fail evaluation' >&2
  exit 1
fi
grep -F 'Locked artifacts: verified' rendered-evaluation.md
grep -F 'informational; never a replay verdict' rendered-evaluation.md
```

The lock verifies the two fixture files, while the evaluation fails because no
replay plan or runtime artifacts are present. That distinction is intentional:
this sample claims no Antithesis equivalence and provides no runtime proof.

## 3. Relock a deliberate edit

Only after intentionally changing the local fixture, regenerate its lock:

```sh
theseus evaluate lock theseus-evaluation.toml
```

Review the lock diff before accepting it.

## 4. Clean up (optional)

```sh
rm -f rendered-evaluation.json rendered-evaluation.md
```
