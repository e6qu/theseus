# Integration guide: Run a container image

Use this path when your system already ships as a container image. Theseus
flattens the image into an initramfs, adds its pivot as `/init`, then boots it
under the same service API. Your image does not need a Theseus guest driver.

## 1. Run a working image

Start with [tutorial 14](../../tutorials/14-container-image/). It uses only a
published Theseus runtime image and the files in its own directory.

For a deterministic operation campaign against an unmodified image, continue
with [tutorial 15](../../tutorials/15-container-campaign/) for HTTP,
[tutorial 17](../../tutorials/17-grpc-campaign/) for standard gRPC health, or
[tutorial 18](../../tutorials/18-container-command/) for an argv command in
the image filesystem.

## 2. Prepare your image

Export it in Docker's image-tar format:

```sh
docker save myimage > guest/service.tar
```

The published Linux runtime exposes the adapter as `theseus-image`. Add the
image and adapter to the same test directory as your manifest:

```toml
[runtime]
firecracker = "runtime/firecracker"
image_adapter = "runtime/theseus-image"

[guest]
kernel = "guest/vmlinux"
image = "guest/service.tar"

[container_service.ready]
url = "http://127.0.0.1:8080/health"

[[container_service.assertions]]
name = "health"
url = "http://127.0.0.1:8080/health"
expect_status = 200
body_contains = "ok"
```

`theseus test` writes the bootable initramfs in its run directory and locks
both source artifacts in the replay bundle. It preserves the image command,
environment, and working directory. With `container_service`, its injected
PID 1 waits for the ready URL, performs each HTTP GET assertion, records a
check for each result, then stops the service. The regular test, Compose, and
replay paths then use that locked image; the lower-level Rust API remains
`orchestrator::oci::flatten`.

## Drive an HTTP operation

Run named requests after readiness and before the final assertions. Theseus
records each result as a check, so a failed request remains visible in the
replay bundle:

```toml
[[container_service.operations]]
name = "create_item"
method = "post"
url = "http://127.0.0.1:8080/items"
body = "{\"name\":\"item-42\"}"
expect_status = 201
body_contains = "created"
```

`method` is `get`, `post`, `put`, or `delete`; `get` is the default. Request
bodies are literal UTF-8 text. Keep operation names unique across HTTP and
gRPC assertions.

`ready.attempts` defaults to 50 and `ready.interval_millis` defaults to 100.
Use `http://` URLs with a host, optional port, and path.

## gRPC health checks

For an unmodified gRPC service, use the standard health service instead of an
HTTP endpoint:

```toml
[container_service.grpc_ready]
url = "http://127.0.0.1:50051"
service = "example.Api"

[[container_service.grpc_assertions]]
name = "api_is_serving"
url = "http://127.0.0.1:50051"
service = "example.Api"
expect_status = "serving"
```

Theseus speaks clear-text HTTP/2 prior knowledge and calls
`grpc.health.v1.Health/Check`. `expect_status` is one of `unknown`, `serving`,
`not_serving`, or `service_unknown`. Use this path for a service that exposes
the standard gRPC health protocol; TLS, reflection, and arbitrary RPC calls
are not part of this small adapter.

## Drive a gRPC health operation

Use the same narrow protocol as a named operation after readiness. It works in
a single-image test and is also the operation form that Compose campaigns
lock, minimize, and replay:

```toml
[[container_service.grpc_operations]]
name = "api_health"
url = "http://127.0.0.1:50051"
service = "example.Api"
expect_status = "serving"
```

## Run a command in the image

Run an argv command after readiness when your normal integration check is a
migration, diagnostics command, or client already present in the image:

```toml
[[container_service.shell_operations]]
name = "check_schema"
command = ["/app/migrate", "--check"]
expect_exit = 0
output_contains = "up to date"
output_json = true
environment = { CHECK_MODE = "full" }
```

Theseus calls the argv directly in the image working directory with the image
environment. It captures bounded combined stdout and stderr to evaluate
`output_contains`; it does not evaluate a shell string. Set `output_json` when
the complete command output is one JSON value. Theseus emits a JSON-lines event
with `/event: shell_operation`, `/name`, and the parsed value at `/output`, so
campaign properties can query fields without parsing text.
`environment` overrides named variables from the image only for this command;
Theseus preserves its own serial-channel variable.

For a gRPC-health campaign, put the equivalent request in its Compose operation:

```yaml
- name: api_health
  grpc_health:
    url: http://127.0.0.1:50051
    service: example.Api
    expect_status: serving
```

Theseus serializes the request into the locked topology input, calls the
standard health endpoint through the image pivot, records
`THES:GRPC:operation:<name>:PASS` or `FAIL`, and takes the usual operation
checkpoint. The service does not need an SDK, reflection, TLS, or an arbitrary
RPC adapter.

For a command campaign, use `shell` instead:

```yaml
- name: check_schema
  shell:
    command: ["/app/migrate", "--check"]
    expect_exit: 0
    output_contains: up to date
    output_json: true
    environment: {CHECK_MODE: full}
```

Theseus locks this argv command into the campaign input, runs it after the
boot barrier, records `THES:SHELL:operation:<name>:PASS` or `FAIL`, and takes
the usual operation checkpoint. With `output_json`, use a property predicate
such as `fields: { /event: shell_operation, /output/state: ready }`.

Static and dynamically linked images work when their dependencies are inside
the image. Use the simulated network for deterministic networking.

## Connect image services in Compose

Put image-backed services on a named Compose network. Theseus assigns stable
IPv4 addresses while it locks the topology, brings up each guest `ethN`, and
writes the reachable peer service names into `/etc/hosts` before starting the
image entrypoint. This applies even when an image has no `container_service`
readiness or operation contract. Use the Compose name directly:

```yaml
services:
  api:
    networks: [backplane]
  worker:
    networks: [backplane]
networks:
  backplane: {}
```

An argv operation in `api` can then call `http://worker:8080/health`. The
images do not need `ip`, DHCP, a sidecar, or Theseus code. The locked replay
plan records the selected addresses and peer mappings. See [tutorial
19](../../tutorials/19-compose-image-network/).

Use standard Compose `depends_on` when one image must start after another:

```yaml
services:
  api:
    depends_on:
      worker:
        condition: service_started
```

Theseus resumes dependencies first and waits for their deterministic boot
marker before starting each dependent. `service_healthy` is also available
when the dependency declares a `container_service` readiness check. Dependency
cycles, unknown services, and a healthy condition without that readiness
contract fail while the Compose plan is created.

## Configure an image with Compose environment

Use literal `environment` values in a Compose service to override the image
entrypoint environment:

```yaml
services:
  worker:
    environment:
      LOG_LEVEL: debug
      RETRIES: "3"
```

The values are copied into the locked image contract and take precedence over
same-named image `ENV` values. The list form is also supported when every
entry uses `KEY=value`. Bare names and null values, which would inherit the
host environment, are rejected. `THESEUS_CHANNEL` remains reserved for the
control transport.

See [the control channel](../../control-channel.md) and
[determinism](../../determinism.md).

## Change an image launch with Compose

Use argv lists to replace an image command or entrypoint, and `working_dir` to
set the image process directory:

```yaml
services:
  worker:
    entrypoint: [/bin/worker]
    command: [--listen, 8080]
    working_dir: /srv/worker
```

`command` replaces the image `Cmd` and retains its image `Entrypoint`. An
explicit `entrypoint` replaces the image entrypoint and drops the image `Cmd`;
the supplied `command` then supplies its arguments. Theseus supports only argv
lists, never shell strings. A Compose `entrypoint` program and `working_dir`
must be absolute paths. All three values are locked into the derived initramfs
and reused by replay. They apply only to image-backed services.

A bare program in an image or Compose `command`, such as `httpd`, resolves
through the locked image `PATH`, as it does under Docker. No host `PATH` is
consulted.

See [tutorial 20](../../tutorials/20-compose-image-launch/).
