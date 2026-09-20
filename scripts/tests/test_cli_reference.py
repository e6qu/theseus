#!/usr/bin/env python3
"""Keep the CLI reference's command list equal to the built-in usage text."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MAIN = (ROOT / "cli/src/main.rs").read_text()
README = (ROOT / "cli/README.md").read_text()


def usage_commands(source: str) -> list[str]:
    """Command lines from a usage text: one entry per command, continuations joined."""
    commands: list[str] = []
    for line in source.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("theseus "):
            commands.append(" ".join(stripped.split()))
        elif commands and stripped.startswith("["):
            commands[-1] = " ".join([commands[-1], *stripped.split()])
    return commands


def built_in_usage() -> str:
    marker = 'const USAGE: &str = "'
    start = MAIN.index(marker) + len(marker)
    end = MAIN.index('";', start)
    return MAIN[start:end].replace("\\n", "\n")


def readme_commands() -> str:
    heading = README.index("## Commands")
    block_start = README.index("```sh", heading) + len("```sh\n")
    block_end = README.index("```", block_start)
    return README[block_start:block_end]


def main() -> None:
    built = usage_commands(built_in_usage())
    documented = usage_commands(readme_commands())
    missing = [command for command in built if command not in documented]
    stale = [command for command in documented if command not in built]
    assert not missing, f"commands missing from cli/README.md: {missing}"
    assert not stale, f"commands in cli/README.md are not in the built-in usage: {stale}"
    assert built == documented, "command order differs between usage and cli/README.md"


if __name__ == "__main__":
    main()
