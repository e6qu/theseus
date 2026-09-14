#!/usr/bin/env python3
"""Keep release smoke checks outside nested container-shell quoting."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text()


def main() -> None:
    assert "/usr/local/bin/theseus-image pivot | grep" not in WORKFLOW
    assert "pivot_sha256=$(sha256sum /opt/theseus/pivot" not in WORKFLOW
    assert WORKFLOW.count("pivot_metadata=$(docker run") == 2
    assert WORKFLOW.count("pivot_manifest=$(docker run") == 2
    assert WORKFLOW.count("pivot_file_sha256=$(docker run") == 2
    assert WORKFLOW.count("pivot_file_sha256=${pivot_file_sha256%% *}") == 2
    assert WORKFLOW.count("jq -r .architecture") == 2
    assert WORKFLOW.count("jq -r .sha256") == 6
    assert WORKFLOW.count("jq -r .source_commit") == 2


if __name__ == "__main__":
    main()
