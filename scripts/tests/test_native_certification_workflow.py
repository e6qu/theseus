#!/usr/bin/env python3
"""Keep native certificates and counterexample archives tied to one SHA."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/certify-deterministic-runtime.yml").read_text()
VALIDATION = (ROOT / "scripts/run_native_validation.sh").read_text()
COUNTEREXAMPLE = (ROOT / "scripts/run_native_counterexample.sh").read_text()
CERTIFICATION_INIT = (
    ROOT / "docs/tutorials/11-certify-runtime/service/init"
).read_text()
CERTIFICATION_MANIFEST = (
    ROOT / "docs/tutorials/11-certify-runtime/service/theseus.toml"
).read_text()
CERTIFICATION_FINISH = (
    ROOT / "docs/tutorials/11-certify-runtime/service/finish.c"
).read_text()


def main() -> None:
    source_ci = (ROOT / ".github/workflows/ci.yml").read_text()
    assert 'sh scripts/run_native_validation.sh || validation_status=$?' in source_ci
    assert 'sh scripts/run_native_counterexample.sh || counterexample_status=$?' in source_ci
    assert 'test "$validation_status" -eq 0' in source_ci
    assert 'test "$counterexample_status" -eq 0' in source_ci
    assert "cargo build -p firecracker --release" in source_ci
    for package in ("cli", "topology-runner", "image-runner"):
        assert f"cargo build --manifest-path {package}/Cargo.toml --locked --release" in source_ci
        assert f"{package}/target/release/" in source_ci
    assert "firecracker/build/cargo_target/release/firecracker" in source_ci
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
    assert "grep -F" not in WORKFLOW
    assert "THES:M:42" in CERTIFICATION_INIT
    assert "exec /bin/finish" in CERTIFICATION_INIT
    assert "RB_AUTOBOOT" in CERTIFICATION_FINISH
    assert "tcdrain(serial)" in CERTIFICATION_FINISH
    assert CERTIFICATION_INIT.count("stty -F /dev/ttyS0 -echo -opost") == 1
    assert CERTIFICATION_INIT.count("read -r command < /dev/ttyS0") == 1
    assert source_ci.count("gcc -static -O2 -Wall -Wextra -Werror service/finish.c") == 1
    assert WORKFLOW.count("gcc -static -O2 -Wall -Wextra -Werror service/finish.c") == 1
    assert 'checkpoint = "finished"' in CERTIFICATION_MANIFEST
    assert "max_rounds = 10000000" in CERTIFICATION_MANIFEST
    assert WORKFLOW.count("sh scripts/run_native_counterexample.sh") == 1
    assert 'THESEUS_IMAGE: ghcr.io/e6qu/theseus:${{ needs.resolve.outputs.tag }}-${{ matrix.arch }}' in WORKFLOW
    assert "--expect-counterexample distributed_lost_update_is_unreachable" not in WORKFLOW
    assert '"network":"recovered"' not in WORKFLOW
    assert "docs/tutorials/30-multiservice-lost-update" in WORKFLOW
    assert "scripts/native_runtime_evidence.py seal" in WORKFLOW
    assert "scripts/run_native_validation.sh" in WORKFLOW
    assert "name: Retain failed native certification diagnostics" in WORKFLOW
    assert "if: failure()" in WORKFLOW
    assert "name: failed-native-certification-${{ matrix.arch }}" in WORKFLOW
    assert "name: Make failed native certification diagnostics readable" in WORKFLOW
    assert 'sudo chmod -R a+rX "$path"' in WORKFLOW
    assert "include-hidden-files: true" in WORKFLOW
    assert "docs/tutorials/30-multiservice-lost-update/campaign/" in WORKFLOW
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
    assert "theseus evaluate capture rerun" in VALIDATION
    assert "theseus evaluate evaluation/theseus-evaluation.toml" in VALIDATION
    assert VALIDATION.index("theseus compose replay campaign --output rerun") < VALIDATION.index(
        "theseus evaluate capture rerun"
    )
    assert "execution_decisions" in VALIDATION
    assert 'status\\\": \\\"(same|diverged)' in VALIDATION
    assert "scripts/runtime_validation_evidence.py" in VALIDATION
    assert "scripts/reproducible_tar.py" in VALIDATION
    assert " jq " not in VALIDATION
    # The released path executes the counterexample as root inside a
    # privileged container. It must hand the generated outputs back to the
    # invoking user, or the hosted seal step cannot extend minimized/.
    assert COUNTEREXAMPLE.count('sh -ec "$commands"') == 1
    assert 'sh -ec "$commands$ownership"' in COUNTEREXAMPLE
    assert '-e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)"' in COUNTEREXAMPLE
    assert 'chown -R "$HOST_UID:$HOST_GID" "$output"' in COUNTEREXAMPLE
    assert (
        "for output in plan.json runtime-pivot.json campaign minimized rerun retained; do"
        in COUNTEREXAMPLE
    )
    assert COUNTEREXAMPLE.index("HOST_UID") > COUNTEREXAMPLE.index(
        "cp /opt/theseus/pivot.json runtime-pivot.json"
    )
    assert "chown" not in COUNTEREXAMPLE.split("commands='")[0]
    # The released validation container must also restore invoking-user
    # ownership: docker save writes 0600 image archives and fs::copy
    # preserves that mode, so root-owned bundle artifacts would be unreadable
    # to the host-side evidence copies.
    assert VALIDATION.count("restore_host_ownership") == 2
    assert 'sh -ec \'chown -R "$HOST_UID:$HOST_GID" /tutorial\'' in VALIDATION
    assert '-e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)"' in VALIDATION
    assert VALIDATION.index("restore_host_ownership \"$tutorial\"") > VALIDATION.index(
        "docker run --rm --privileged"
    )


if __name__ == "__main__":
    main()
