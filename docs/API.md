# API Reference

## Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/v1/exec` | POST | Execute code in an isolated sandbox |
| `/v1/exec/batch` | POST | Execute multiple snippets in parallel |
| `/v1/health` | GET | Template status and readiness |
| `/v1/metrics` | GET | Prometheus-format metrics |

## POST /v1/exec

Execute code in a freshly forked VM sandbox.

**Request:**

```json
{
  "code": "print(1 + 1)",
  "language": "python",
  "timeout_seconds": 30
}
```

- `code` (string, required): Code to execute
- `language` (string, optional): `python` (default), `node`, or `javascript`
- `timeout_seconds` (integer, optional): Execution timeout, default 30

**Response:**

```json
{
  "id": "019cf684-1fd5-73c0-9299-52253f9aa79c",
  "stdout": "2\n",
  "stderr": "",
  "exit_code": 0,
  "fork_time_ms": 0.75,
  "exec_time_ms": 7.2,
  "total_time_ms": 8.0
}
```

**Example:**

```bash
curl -X POST localhost:8080/v1/exec \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer zb_live_...' \
  -d '{"code": "import numpy; print(numpy.random.rand(3))", "language": "python"}'
```

## POST /v1/exec/batch

Execute multiple code snippets in parallel. Each snippet runs in its own isolated VM fork.

**Request:**

```json
{
  "executions": [
    {"code": "print(1)", "language": "python"},
    {"code": "console.log(2)", "language": "node"}
  ]
}
```

**Response:**

```json
{
  "results": [
    {"id": "...", "stdout": "1\n", "stderr": "", "exit_code": 0, "fork_time_ms": 0.72, "exec_time_ms": 6.8, "total_time_ms": 7.5},
    {"id": "...", "stdout": "2\n", "stderr": "", "exit_code": 0, "fork_time_ms": 0.81, "exec_time_ms": 12.1, "total_time_ms": 12.9}
  ]
}
```

## GET /v1/health

Returns template status and readiness.

**Response:**

```json
{
  "status": "ok",
  "templates": {
    "python": {"ready": true, "memory_mb": 256},
    "node": {"ready": true, "memory_mb": 256}
  }
}
```

## GET /v1/metrics

Returns Prometheus-format metrics including fork time histograms, exec time histograms, request counts, and error rates.

## Authentication

Place API keys in `api_keys.json` (or set `ZEROBOOT_API_KEYS_FILE` env var):

```json
["zb_live_key1", "zb_live_key2"]
```

Send keys via the `Authorization` header:

```
Authorization: Bearer zb_live_key1
```

- If no keys file exists, auth is disabled
- Invalid or missing keys return **HTTP 401**
- Rate limited at **100 req/s per key** (HTTP 429)

## Filesystem Access

Sandboxes have a full read-write filesystem backed by the rootfs image specified at template
creation time. Each fork gets an independent overlay so writes are isolated:

```python
# Works inside CODE: commands
import os
os.makedirs("/tmp/mydir", exist_ok=True)
with open("/tmp/mydir/output.txt", "w") as f:
    f.write("hello from sandbox")
print(open("/tmp/mydir/output.txt").read())  # hello from sandbox
```

Writes are discarded when the fork ends — the base image is never modified.
To persist outputs, include them in the stdout response.
