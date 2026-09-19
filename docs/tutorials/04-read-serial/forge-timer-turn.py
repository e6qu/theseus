#!/usr/bin/env python3
"""Forge one kernel-timer turn into a replay bundle's execution evidence.

Insert a lapic-timer decision into the recorded machine stream of a copy of
a held-timer bundle and rebuild the evidence digests exactly as the evidence
contract defines them, so the only tampering is the forged turn itself.
Replay must reject the bundle.
"""

import collections
import hashlib
import json
import sys
from pathlib import Path

FORGED_RECORD = "vcpu:0:interrupt:lapic-timer:250"


def ledger(decisions):
    hasher = hashlib.sha256()
    tail = collections.deque()
    count = 0
    for decision in decisions:
        raw = decision.encode()
        hasher.update(len(raw).to_bytes(8, "little"))
        hasher.update(raw)
        count += 1
        if len(tail) == 32:
            tail.popleft()
        tail.append(decision)
    return {"decisions": count, "sha256": hasher.hexdigest(), "tail": list(tail)}


def main() -> None:
    bundle = Path(sys.argv[1])
    evidence_path = bundle / "execution.json"
    evidence = json.loads(evidence_path.read_text())
    trace = evidence["machine_execution_trace"]
    trace.insert(1, FORGED_RECORD)
    evidence["machine_execution_ledger"] = ledger(trace)
    local = collections.defaultdict(list)
    for record in trace:
        if record.startswith("vcpu:"):
            vcpu, decision = record[5:].split(":", 1)
            local[int(vcpu)].append(decision)
    evidence["execution_ledgers"] = [
        ledger(local.get(index, [])) for index in range(len(evidence["execution_ledgers"]))
    ]
    evidence_path.write_text(json.dumps(evidence))


if __name__ == "__main__":
    main()
