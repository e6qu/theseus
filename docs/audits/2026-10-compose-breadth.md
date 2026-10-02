# Compose breadth audit - 2026-10

The documented Compose subset, audited against the Compose specification's
common service fields. Every unsupported field is recorded with the reason
it is out of scope, so breadth work picks from an honest list instead of
guessing. Unknown fields are rejected by name (`deny_unknown_fields`);
nothing is silently ignored.

## Supported

`x-theseus` (manifest, campaign), `networks` (named deterministic
backplanes), `depends_on`, `environment` (literal values), `env_file`
(single and list), `command`, `entrypoint`, `working_dir`, `user`,
`configs`, `secrets` (file-based), `volumes` (memory-backed),
`healthcheck`, `hostname`, `extra_hosts`, `cpus`, `mem_limit`,
`deploy.resources.limits`, `read_only`, `tmpfs`.

## Unsupported, with reasons

| Field | Reason |
| --- | --- |
| `ports` | Host port publishing is host-state. Theseus topologies reach services through deterministic named networks, never host sockets. |
| `network_mode: host` | Host networking depends on the machine. Only named deterministic networks are accepted. |
| `restart` | Host-level lifecycle policy sits outside the deterministic virtual clock. Deterministic restarts exist as declared faults. |
| `privileged`, `devices` | Host kernel and device access is the VM's job, never the guest's. |
| `volumes` bind mounts and named host volumes | Host paths are not portable and break replay lockability. Memory-backed `volumes` are supported. |
| `secrets: external` | Host-provided secrets cannot be locked. File-based secrets are supported. |
| `build` | Image builds happen outside Theseus; the input is a built image (`docker save`). |
| `deploy.replicas` | One deterministic timeline per named service. Scale by declaring distinct services with distinct roles. |
| `depends_on.condition` | Readiness is the healthcheck and ready-checkpoint contract, not Docker's condition waits. |
| `environment` host inheritance (`KEY` without `=`) | Inherited values would make a locked plan depend on the machine that wrote it. Literal values only. |
| `pid`, `uts`, `ipc`, `cgroup` namespace sharing | Host namespace sharing is host-state by definition. |
| `labels`, `logging`, `profiles` | Host tooling metadata and host-side selection; they do not affect the deterministic timeline. |
| `stdin_open`, `tty` | The UART control channel is the deterministic input surface, not host TTYs. |
| `stop_grace_period`, `stop_signal` | Shutdown is deterministic at the barrier, not signal-mediated on the host. |

Adding a field means adding its deterministic contract: a field is
supported when its behavior is fully specified by the locked plan and the
guest input, never by the host.
