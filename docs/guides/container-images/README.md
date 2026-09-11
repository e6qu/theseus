# Integration guide: Run a container image

Use this path when your system already ships as a container image. Theseus
flattens the image into an initramfs, adds its pivot as `/init`, then boots it
under the same service API. Your image does not need a Theseus guest driver.

## 1. Run a working image

Start with [tutorial 14](../../tutorials/14-container-image/). It uses only a
published Theseus runtime image and the files in its own directory.

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

Static and dynamically linked images work when their dependencies are inside
the image. Use the simulated network for deterministic networking.

See [the control channel](../../control-channel.md) and
[determinism](../../determinism.md).
