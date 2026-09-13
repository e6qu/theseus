#!/usr/bin/env python3
"""Check relative links in active Theseus Markdown."""

from pathlib import Path
from urllib.parse import unquote
import re

ROOT = Path(__file__).resolve().parents[2]
SKIP = {"firecracker", "target"}

for document in ROOT.rglob("*.md"):
    relative = document.relative_to(ROOT)
    if any(part in SKIP for part in relative.parts):
        continue
    text = document.read_text(errors="ignore")
    for target in re.findall(r"\[[^]]*\]\(([^)]+)\)", text):
        target = target.strip().split(" ", 1)[0]
        if not target or target.startswith(("#", "http://", "https://", "mailto:")):
            continue
        path_text = unquote(target.split("#", 1)[0])
        resolved = (document.parent / path_text).resolve()
        try:
            resolved.relative_to(ROOT.resolve())
        except ValueError as error:
            raise AssertionError(f"{document}: link escapes repository: {target}") from error
        assert resolved.exists(), f"{document}: broken relative link: {target}"
