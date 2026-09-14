#!/usr/bin/env python3
"""Keep native certificates and counterexample archives tied to one SHA."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/certify-deterministic-runtime.yml").read_text()


def main() -> None:
    assert WORKFLOW.count("- arch: amd64") == 1
    assert WORKFLOW.count("- arch: arm64") == 1
    assert "ref: ${{ inputs.tag }}" in WORKFLOW
    assert 'test "$(git rev-parse --short=12 HEAD)" = "$TAG"' in WORKFLOW
    assert 'docker pull "$IMAGE:$TAG-$ARCH"' in WORKFLOW
    assert WORKFLOW.count('docker build --load --platform "linux/$ARCH"') == 2
    assert "docs/tutorials/30-multiservice-lost-update" in WORKFLOW
    assert WORKFLOW.count("--expect-counterexample distributed_lost_update_is_unreachable") == 2
    assert 'theseus compose replay minimized --output rerun' in WORKFLOW
    assert "backplane:partition@setup" in WORKFLOW
    assert "backplane:heal@probe_partition" in WORKFLOW
    assert '"dropped"' in WORKFLOW
    assert "rerun/services/*/result.json" in WORKFLOW
    assert WORKFLOW.count("rerun/topology-result.json") >= 3
    assert "rerun/services/writer-a/serial.log" in WORKFLOW
    assert 'cp /opt/theseus/pivot.json runtime-pivot.json' in WORKFLOW
    assert '"format": "theseus-counterexample-proof-v1"' in WORKFLOW
    assert '"$tutorial/minimized/evidence"' in WORKFLOW
    assert '"$tutorial/minimized/source"' in WORKFLOW
    assert '--output "$archive" "$tutorial/minimized"' in WORKFLOW
    assert "multiservice-counterexample-${ARCH}.tar.gz" in WORKFLOW
    assert "multiservice-counterexample-${{ matrix.arch }}.tar.gz" in WORKFLOW


if __name__ == "__main__":
    main()
