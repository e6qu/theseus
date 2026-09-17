#!/usr/bin/env python3
"""Keep native certificates and counterexample archives tied to one SHA."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/certify-deterministic-runtime.yml").read_text()
VALIDATION = (ROOT / "scripts/run_native_validation.sh").read_text()
CERTIFICATION_INIT = (
    ROOT / "docs/tutorials/11-certify-runtime/service/init"
).read_text()
CERTIFICATION_MANIFEST = (
    ROOT / "docs/tutorials/11-certify-runtime/service/theseus.toml"
).read_text()


def main() -> None:
    source_ci = (ROOT / ".github/workflows/ci.yml").read_text()
    assert 'sh scripts/run_native_validation.sh || validation_status=$?' in source_ci
    assert 'sh scripts/run_native_counterexample.sh || counterexample_status=$?' in source_ci
    assert 'test "$validation_status" -eq 0' in source_ci
    assert 'test "$counterexample_status" -eq 0' in source_ci
    assert "workflow_run:" in WORKFLOW
    assert "workflows: [publish-runtime]" in WORKFLOW
    assert "default: amd64" in WORKFLOW
    assert '"ubuntu-24.04"' in WORKFLOW
    assert '"self-hosted","Linux","ARM64","kvm"' in WORKFLOW
    assert "matrix: ${{ fromJSON(needs.resolve.outputs.matrix) }}" in WORKFLOW
    assert "name: resolve published SHA" in WORKFLOW
    assert "ref: ${{ needs.resolve.outputs.commit }}" in WORKFLOW
    assert "fetch-depth: 0" in WORKFLOW
    assert "^[0-9a-f]{12}$" in WORKFLOW
    assert 'test "$(jq -r .isDraft "$release/release.json")" = false' in WORKFLOW
    assert 'git merge-base --is-ancestor "$commit" origin/main' in WORKFLOW
    assert 'test "$(jq -r .source.commit "$inputs")" = "$commit"' in WORKFLOW
    assert WORKFLOW.count("gh attestation verify") >= 2
    assert 'test "$(git rev-parse --short=12 HEAD)" = "$TAG"' in WORKFLOW
    assert "sudo chmod 666 /dev/kvm" in WORKFLOW
    assert WORKFLOW.index("sudo chmod 666 /dev/kvm") < WORKFLOW.index(
        'kvm_api=$(python3 -c'
    )
    assert "fcntl.ioctl(fd, 0xAE00, 0)" in WORKFLOW
    assert 'docker pull "$IMAGE:$TAG-$ARCH"' in WORKFLOW
    assert "theseus compose plan > /tutorial/plan.json" in WORKFLOW
    assert "--plan /tutorial/plan.json --output /tutorial/certificate" in WORKFLOW
    assert "THES:M:42" in CERTIFICATION_INIT
    assert "reboot -f" in CERTIFICATION_INIT
    assert "poweroff -f" not in CERTIFICATION_INIT
    assert CERTIFICATION_INIT.count("stty -F /dev/ttyS0 -echo -opost") == 2
    assert CERTIFICATION_INIT.index("echo finished") < CERTIFICATION_INIT.rindex("stty -F")
    assert CERTIFICATION_INIT.rindex("stty -F") < CERTIFICATION_INIT.index("reboot -f")
    assert "max_rounds = 10000000" in CERTIFICATION_MANIFEST
    assert WORKFLOW.count('docker build --load --platform "linux/$ARCH"') == 2
    assert "docs/tutorials/30-multiservice-lost-update" in WORKFLOW
    assert WORKFLOW.count("--expect-counterexample distributed_lost_update_is_unreachable") == 2
    assert 'theseus compose replay minimized --output rerun' in WORKFLOW
    assert "backplane:partition@setup" in WORKFLOW
    assert "backplane:heal@probe_partition" in WORKFLOW
    assert "dropped" in WORKFLOW
    assert "rerun/services/*/result.json" in WORKFLOW
    assert WORKFLOW.count("rerun/topology-result.json") >= 3
    assert "rerun/services/writer-a/serial.log" in WORKFLOW
    assert 'cp /opt/theseus/pivot.json runtime-pivot.json' in WORKFLOW
    assert "scripts/native_runtime_evidence.py seal" in WORKFLOW
    assert "scripts/run_native_validation.sh" in WORKFLOW
    assert '"$validation/fixed-plan"' in VALIDATION
    assert "execution-error.json" in WORKFLOW
    assert 'grep -F "\\\"host:serial_input:"' in VALIDATION
    assert 'grep -F "\\\"vcpu:0:interrupt:serial:"' in VALIDATION
    assert 'grep -F "\\\"vcpu:0:interrupt:virtio-"' in VALIDATION
    assert "runtime-validation-${ARCH}.tar.gz" in WORKFLOW
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
    assert '--architectures "${architectures[@]}"' in WORKFLOW
    assert "evidence verify" in WORKFLOW
    assert "subject-path: native-evidence/*" in WORKFLOW
    assert "native-evidence/*-runtime-certificate-*.json" in WORKFLOW
    assert "native-evidence/*-multiservice-counterexample-*.tar.gz" in WORKFLOW
    assert "native-evidence/*-runtime-validation-*.tar.gz --clobber" in WORKFLOW
    assert WORKFLOW.index("*-runtime-validation-*.tar.gz --clobber") < WORKFLOW.index(
        '"native-evidence/theseus-${TAG}-native-evidence.json" --clobber'
    )
    for tutorial in (
        "14-container-image",
        "31-c-basic-block-coverage",
        "33-search-thread-schedules",
        "35-control-pthread-synchronization",
        "41-reject-execution-divergence",
    ):
        assert tutorial in VALIDATION
    assert VALIDATION.count("theseus compose replay") == 4
    assert "theseus replay --output work/rerun work/replay" in VALIDATION
    assert 'machine_replay = "host_inputs"' in (
        ROOT / "docs/tutorials/14-container-image/theseus.toml"
    ).read_text()
    for tutorial in (
        "31-c-basic-block-coverage",
        "33-search-thread-schedules",
        "35-control-pthread-synchronization",
    ):
        dockerfile = (
            ROOT / "docs/tutorials" / tutorial / "service/Dockerfile"
        ).read_text()
        assert "FROM scratch" in dockerfile
        assert 'cp --parents "$dependency" /rootfs' in dockerfile
    assert "cmp work/replay/execution.json work/rerun/execution.json" not in VALIDATION
    assert "theseus compare campaign rerun" in VALIDATION
    assert "theseus evaluate capture campaign" in VALIDATION
    assert "theseus evaluate evaluation/theseus-evaluation.toml" in VALIDATION
    assert "execution_decisions" in VALIDATION
    assert 'status\\\": \\\"same' in VALIDATION
    assert "scripts/runtime_validation_evidence.py" in VALIDATION
    assert "scripts/reproducible_tar.py" in VALIDATION
    assert " jq " not in VALIDATION


if __name__ == "__main__":
    main()
