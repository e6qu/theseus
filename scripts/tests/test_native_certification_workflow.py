#!/usr/bin/env python3
"""Keep native certificates and counterexample archives tied to one SHA."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/certify-deterministic-runtime.yml").read_text()


def main() -> None:
    assert WORKFLOW.count("- arch: amd64") == 1
    assert WORKFLOW.count("- arch: arm64") == 1
    assert "name: resolve published SHA" in WORKFLOW
    assert "ref: ${{ needs.resolve.outputs.commit }}" in WORKFLOW
    assert "fetch-depth: 0" in WORKFLOW
    assert "^[0-9a-f]{12}$" in WORKFLOW
    assert 'test "$(jq -r .isDraft "$release/release.json")" = false' in WORKFLOW
    assert 'git merge-base --is-ancestor "$commit" origin/main' in WORKFLOW
    assert 'test "$(jq -r .source.commit "$inputs")" = "$commit"' in WORKFLOW
    assert WORKFLOW.count("gh attestation verify") >= 2
    assert 'test "$(git rev-parse --short=12 HEAD)" = "$TAG"' in WORKFLOW
    assert "fcntl.ioctl(fd, 0xAE00, 0)" in WORKFLOW
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
    assert "scripts/native_runtime_evidence.py seal" in WORKFLOW
    assert '--runtime-tag "$TAG-$ARCH"' in WORKFLOW
    assert '--kvm-api-version "$kvm_api"' in WORKFLOW
    assert '"$evidence/runtime-certificate.json"' in WORKFLOW
    assert '"$tutorial/minimized/evidence"' in WORKFLOW
    assert '"$tutorial/minimized/source"' in WORKFLOW
    assert '--output "$archive" "$tutorial/minimized"' in WORKFLOW
    assert "multiservice-counterexample-${ARCH}.tar.gz" in WORKFLOW
    assert "name: native-evidence-${{ matrix.arch }}" in WORKFLOW
    assert "pattern: native-evidence-*" in WORKFLOW
    assert "merge-multiple: true" in WORKFLOW
    assert "scripts/native_runtime_evidence.py index" in WORKFLOW
    assert "evidence verify" in WORKFLOW
    assert "subject-path: native-evidence/*" in WORKFLOW
    assert "native-evidence/*-runtime-certificate-*.json" in WORKFLOW
    assert "native-evidence/*-multiservice-counterexample-*.tar.gz --clobber" in WORKFLOW
    assert WORKFLOW.index("*-multiservice-counterexample-*.tar.gz --clobber") < WORKFLOW.index(
        '"native-evidence/theseus-${TAG}-native-evidence.json" --clobber'
    )


if __name__ == "__main__":
    main()
