#!/usr/bin/env python3
"""Smoke-test the read-only serve surface over a real TCP server.

Builds recorded fixtures, writes a versioned registry, starts
`theseus serve --index` on a local port, and asserts every documented
route answers with its documented status. Run from the repository root;
the script launches the CLI binary through cargo.
"""

import io
import json
import socket
import tarfile
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RESULT = json.dumps({
    "format": "theseus-compose-campaign-result-v1",
    "status": "passed",
    "driver": "chooser",
    "guidance": "GUIDANCE",
    "generated_candidates": 12,
    "runs": [
        {
            "index": 0,
            "operations": ["write"],
            "status": "passed",
            "timeline": [
                {
                    "id": "op-000-write",
                    "operation": "write",
                    "service": "chooser",
                    "round": 7,
                    "moment": "7000@input-hash",
                    "serial_delta": {
                        "chooser": {
                            "bytes": 6,
                            "sha256": "delta-hash",
                            "excerpt": "write\\n",
                            "omitted_bytes": 0,
                        }
                    },
                    "events": {"chooser": ['{"event":"request","seq":1}']},
                }
            ],
        }
    ],
    "properties": [
        {"name": "consistent_read", "kind": "always", "status": "passed", "detail": "1 of 1"}
    ],
})
PLAN = json.dumps({
    "format": "theseus-compose-plan-v1",
    "campaign": {"driver": "chooser", "operations": [{"name": "write"}], "max_runs": 8},
})
PROGRESS = "\n".join([
    '{"format":"theseus-progress-v1","completed":1,"index":0,"status":"passed"}',
    '{"format":"theseus-run-record-v1","index":0,"status":"passed","operations":["write"],"faults":[],"selection":"first"}',
    '{"format":"theseus-checkpoint-ledger-v1","nodes":2,"reuses":1,"prefix_captures":1,"prefix_restores":0,"retained_memory_bytes":1048576}',
]) + "\n"
EXPLORATION = json.dumps({
    "format": "theseus-result-v1",
    "status": "passed",
    "nodes": [{
        "search_index": 1,
        "id": 2,
        "parent": None,
        "depth": 1,
        "seed": 7,
        "seed_path": [1],
        "entropy_probe_hex": "aa",
        "markers_hex": "ff",
        "dirty_pages": 2,
        "serial_log": "serial/1.log",
    }],
})


def write_bundle(root: Path, name: str, guidance: str, candidates: int = 12) -> Path:
    bundle = root / name
    (bundle / "serial").mkdir(parents=True)
    (bundle / "campaign-result.json").write_text(
        RESULT.replace("GUIDANCE", guidance).replace("CANDIDATES_PLACEHOLDER", "")
        .replace('"generated_candidates": 12', f'"generated_candidates": {candidates}')
    )
    (bundle / "replay-plan.json").write_text(PLAN)
    (bundle / "serial" / "1.log").write_bytes(b"ready\n")
    (bundle / "progress.jsonl").write_text(PROGRESS)
    return bundle


def write_exploration(root: Path, name: str) -> Path:
    bundle = root / name
    bundle.mkdir(parents=True)
    (bundle / "result.json").write_text(EXPLORATION)
    (bundle / "explore-plan.json").write_text(
        json.dumps({"format": "theseus-explore-plan-v1", "max_depth": 2})
    )
    return bundle


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def serve(arguments: list[str]) -> subprocess.Popen:
    return subprocess.Popen(
        ["cargo", "run", "--quiet", "--locked", "--bin", "theseus", "--", "serve", *arguments],
        cwd=ROOT / "cli",
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def fetch(port: int, path: str, method: str = "GET", headers: dict = None) -> tuple[int, str, bytes]:
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method)
    for name, value in (headers or {}).items():
        request.add_header(name, value)
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            body = response.read()
            declared = response.headers.get("Content-Length")
            assert declared is None or int(declared) == len(body), (
                f"{path}: Content-Length {declared} != {len(body)} bytes"
            )
            return response.status, response.headers.get("Content-Type", ""), body
    except urllib.error.HTTPError as error:
        return error.code, error.headers.get("Content-Type", ""), error.read()


def wait_until_ready(port: int, process: subprocess.Popen) -> None:
    deadline = time.time() + 120
    while time.time() < deadline:
        if process.poll() is not None:
            raise AssertionError("serve exited before answering")
        try:
            status, _, _ = fetch(port, "/")
            if status == 200:
                return
        except (ConnectionError, urllib.error.URLError):
            time.sleep(1)
    raise AssertionError("serve never answered")


def expect(port: int, path: str, status: int, marker: str = None, method: str = "GET") -> None:
    got, content_type, body = fetch(port, path, method)
    assert got == status, f"{method} {path}: expected {status}, got {got}"
    if marker is not None:
        assert marker in body.decode("utf-8", "replace"), f"{path}: missing {marker!r}"


def main() -> None:
    directory = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp")
    root = directory / "theseus-serve-smoke"
    if root.exists():
        subprocess.run(["rm", "-rf", str(root)], check=True)
    root.mkdir(parents=True)
    write_bundle(root, "campaign-unified", "unified")
    write_bundle(root, "campaign-coverage", "coverage")
    write_bundle(root, "campaign-drift", "unified", candidates=13)
    write_exploration(root, "exploration")
    (root / "registry.json").write_text(json.dumps({
        "format": "theseus-serve-registry-v1",
        "campaigns": [
            {"name": "unified", "directory": "campaign-unified"},
            {"name": "coverage", "directory": "campaign-coverage"},
            {"name": "tree", "directory": "exploration"},
        ],
    }))
    (root / "broken.json").write_text(json.dumps({
        "format": "theseus-serve-registry-v1",
        "campaigns": [{"name": "gone", "directory": "nowhere"}],
    }))

    # A registry naming a missing directory fails startup.
    broken = serve(["--index", str(root / "broken.json"), "--address", "127.0.0.1:0"])
    try:
        assert broken.wait(timeout=300) != 0, "broken registry must fail startup"
    finally:
        broken.terminate()

    port = free_port()
    server = serve(["--index", str(root / "registry.json"), "--address", f"127.0.0.1:{port}"])
    try:
        wait_until_ready(port, server)
        status, _, body = fetch(port, "/routes")
        assert status == 200, "routes manifest"
        manifest = json.loads(body)
        assert manifest["format"] == "theseus-serve-routes-v1", manifest["format"]
        documented = " ".join(route["path"] for route in manifest["routes"])
        expect(port, "/", 200, "/unified/report")
        expect(port, "/unified/result", 200, "theseus-compose-campaign-result-v1")
        expect(port, "/unified/plan", 200, "theseus-compose-plan-v1")
        expect(port, "/unified/report", 200)
        expect(port, "/unified/report.html", 200, "<!doctype html>")
        expect(port, "/unified/progress", 200, "theseus-checkpoint-ledger-v1")
        # Incremental readers rely on the prefix property: the journal is
        # append-only, so a later read is always a superset of an earlier
        # one. Assert it across two polls.
        _, _, first_read = fetch(port, "/unified/progress")
        _, _, second_read = fetch(port, "/unified/progress")
        assert second_read.startswith(first_read), "journal is not append-only"
        # A suffix range fetches only new bytes: the 206 body concatenates
        # with the earlier prefix into the whole journal.
        cut = len(first_read) // 2
        status, content_type, tail = fetch(
            port, "/unified/progress", headers={"Range": f"bytes={cut}-"}
        )
        assert status == 206, status
        assert content_type.startswith("text/plain"), content_type
        assert first_read[:cut] + tail == first_read, "range suffix mismatch"
        past_end = fetch(port, "/unified/progress", headers={"Range": "bytes=999999-"})
        assert past_end[0] == 416, past_end[0]
        expect(port, "/unified/query/moments", 200, "7000@input-hash")
        expect(port, "/unified/query/events", 200, '\\"event\\":\\"request\\"')
        expect(port, "/unified/query/preceded-by/write", 200)
        expect(port, "/unified/query/moment/7000@input-hash", 200, "op-000-write")
        expect(port, "/unified/query/moment/7000@input-hash?next", 404)

        # The collect route answers one tar archive with the CLI's
        # collected files: boundary record, journal prefix, digest manifest.
        status, content_type, archive = fetch(
            port, "/unified/query/moment/7000@input-hash?collect"
        )
        assert status == 200, status
        assert content_type == "application/x-tar", content_type
        with tarfile.open(fileobj=io.BytesIO(archive)) as bundle:
            names = set(bundle.getnames())
            assert "boundary.json" in names, names
            assert "progress.jsonl" in names, names
            assert "manifest.json" in names, names
            boundary = json.load(bundle.extractfile("boundary.json"))
            assert boundary["moment"] == "7000@input-hash", boundary["moment"]
            journal = bundle.extractfile("progress.jsonl").read().decode().splitlines()
            assert len(journal) == 3, journal
            manifest = json.load(bundle.extractfile("manifest.json"))
            assert manifest["format"] == "theseus-collected-artifacts-v1"
            assert manifest["run"] == 0
        expect(port, "/unified/query/moment/9000@input-hash?collect", 404)
        expect(port, "/unified/tree", 200, "theseus-bundle-tree-v1")
        expect(port, "/unified/serial/1.log", 200, "ready\n")
        expect(port, "/unified/file/serial/1.log", 200, "ready\n")
        expect(port, "/unified/file/serial/../result", 404)
        expect(port, "/unified/nope", 404)
        expect(port, "/unified/result", 405, method="POST")
        expect(port, "/tree/query/nodes", 200, "theseus-exploration-nodes-v1")
        expect(port, "/tree/query/node/1", 200, '"seed_path"')
        expect(port, "/history/properties", 200, "theseus-campaign-property-history-v1")
        expect(port, "/history/assertions", 200, "theseus-assertion-catalog-v1")
        expect(port, "/history/events", 200, "theseus-event-history-v1")
        expect(port, "/compare?campaigns=unified,coverage", 200,
               "theseus-guidance-comparison-v1")
        expect(port, "/compare?campaigns=unified,campaign-drift", 404)
        expect(port, "/compare?campaigns=unified,nope", 404)
        expect(port, "/compare", 400)
        for shape in [
            "/<name>/result",
            "/<name>/progress",
            "/<name>/tree",
            "/<name>/query/moments",
            "/<name>/query/moment/<moment>?collect",
            "/<name>/query/node/<seed-path>",
            "/history/events?service=NAME",
            "/compare?campaigns=a,b",
        ]:
            assert shape in documented, f"manifest missing {shape}"
    finally:
        server.terminate()
        server.wait(timeout=60)

    # Directory discovery serves every child bundle without a registry.
    port = free_port()
    guidance = root / "guidance"
    guidance.mkdir()
    for name in ["campaign-unified", "campaign-coverage"]:
        subprocess.run(["cp", "-R", str(root / name), str(guidance / name)], check=True)
    server = serve([str(guidance), "--address", f"127.0.0.1:{port}"])
    try:
        wait_until_ready(port, server)
        expect(port, "/", 200, "/campaign-unified/report")
        expect(port, "/history/events", 200, "theseus-event-history-v1")
    finally:
        server.terminate()
        server.wait(timeout=60)

    subprocess.run(["rm", "-rf", str(root)], check=True)
    print("SERVE-SMOKE-OK")


if __name__ == "__main__":
    main()
