#!/usr/bin/env python3
"""Reject known misleading wording in active Theseus documentation."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ACTIVE = [
    ROOT / "README.md",
    ROOT / "PLAN.md",
    ROOT / "docs",
    ROOT / "cli",
    ROOT / "e2e",
    ROOT / "engine",
    ROOT / "evaluations",
    ROOT / "explorer-runner",
    ROOT / "image-runner",
    ROOT / "orchestrator",
    ROOT / "sdk",
    ROOT / "topology-runner",
]
SUFFIXES = {".md", ".rs", ".yml", ".yaml"}

for root in ACTIVE:
    paths = [root] if root.is_file() else root.rglob("*")
    for path in paths:
        if not path.is_file() or path.suffix not in SUFFIXES:
            continue
        text = path.read_text(errors="ignore")
        for forbidden in (
            "First causal divergence",
            "first causal divergence",
            "Every layer is proven",
            "P6.8 will add",
            "/dev/hwrng",
        ):
            assert forbidden not in text, f"{path}: obsolete claim {forbidden!r}"
