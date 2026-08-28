# AuditReady Server Payloads

Wire format for everything the agent sends to the server. Use this to update
the server code.

Both endpoints receive:

- `Content-Type: application/json`
- `User-Agent: AuditReady/0.1`
- `Authorization: Bearer <token>` (omitted only when the token is empty)

Any 2xx response counts as success. There is no retry queue: failures are
logged and the next interval sends fresh data.

## 1. Agent telemetry — `POST /audit_ready/telemetry`

Sent every `server.interval_seconds` (default 30). The **`disks` field is
new** — always present, but may be an empty array.

```json
{
  "hostname": "Sebrus-Mac-mini.local",
  "os_version": "26.5.2",

  "disks": [
    {
      "mount_point": "/",
      "name": "Macintosh HD",
      "total_bytes": 245107195904,
      "available_bytes": 3650725888
    }
  ],

  "installed_software": {
    "packages": [
      { "name": "Google Chrome", "version": "128.0.6613.85", "source": "Applications" }
    ]
  },

  "pending_updates": [
    { "id": "openssl", "title": "openssl 3.0.2-0ubuntu1.25", "severity": "", "source": "apt", "kb": "" }
  ],

  "iis": {
    "installed": true,
    "sites": [
      {
        "name": "Default Web Site",
        "state": "Started",
        "app_pool": "DefaultAppPool",
        "app_pool_state": "Started",
        "physical_path": "C:\\inetpub\\wwwroot",
        "bindings": ["http *:80:"]
      }
    ]
  },

  "running_processes": {
    "total": 512,
    "flagged": 1,
    "killed": 0,
    "processes": [
      { "pid": 4821, "ppid": 1, "name": "mimikatz.exe", "elevated": true, "verdict": "FLAGGED", "command": "mimikatz.exe" }
    ]
  },

  "network_traffic": {
    "interfaces": [ { "name": "en0", "addresses": ["192.168.1.5"] } ],
    "connections": [
      { "proto": "tcp", "local": "192.168.1.5:51234", "remote": "142.250.72.14:443", "state": "ESTAB", "pid": 4821, "process": "firefox" }
    ],
    "dns_queries": [
      { "ts_ms": 1784307600000, "domain": "example.com", "qtype": "A", "answers": ["93.184.216.34"], "resolver": "192.168.1.1" }
    ],
    "dns_servers": ["192.168.1.1"]
  }
}
```

### Omission rules the deserializer must tolerate

- `pending_updates` — **absent** until the first collection succeeds; after
  that always present (an empty array clears the server snapshot).
- `iis` — same rule; Windows agents only.
- `compliance` — defined in the payload but currently always omitted.
- `connections[].pid` / `connections[].process` and `dns_queries[].pid` /
  `dns_queries[].process` — omitted when unknown (not `null`).
- `connections[].state` is `""` for UDP.
- `running_processes.processes` contains only flagged processes; `total`
  counts all of them.
- `installed_software.packages[].version` is `""` when unknown.

## 2. Client mode — `POST /audit_ready/client-report`

Sent every `client.report_interval_seconds` (default 300) by the per-user
instance (`--mode client`); the first report goes out immediately at startup.
**`disks` is new here too.**

```json
{
  "hostname": "DESKTOP-ABC123",
  "username": "johann",
  "period_start": "2026-08-24T08:00:00Z",
  "period_end": "2026-08-24T08:30:00Z",

  "disks": [
    {
      "mount_point": "C:\\",
      "name": "Windows",
      "total_bytes": 512110190592,
      "available_bytes": 220200960000
    }
  ],

  "changed_files": [
    { "path": "C:\\Users\\johann\\Documents\\report.docx", "size_bytes": 48211, "modified_at": "2026-08-24T08:05:12Z" }
  ],

  "clipboard_events": [
    {
      "ts": "2026-08-24T08:05:12Z",
      "kind": "copy",
      "app": "chrome.exe",
      "size_bytes": 342,
      "sensitive": [ { "kind": "credit_card", "masked": "41**********11" } ],
      "content": null
    }
  ],

  "total_copy_bytes": 61546,
  "sensitive_hits": 1,

  "running_processes": [
    { "name": "chrome.exe", "pid": 1234, "cpu_percent": 2.5, "memory_bytes": 524288000 }
  ],

  "folder_writes": [
    { "folder": "C:\\Users\\johann\\Documents", "write_count": 42, "last_write_at": "2026-08-24T08:05:12Z" }
  ]
}
```

### Notes

- `disks` — identical `DiskEntry` shape as telemetry; always present, may
  be `[]`.
- `clipboard_events[].kind` — `"copy"` or `"paste"`.
- `clipboard_events[].app` — `null` when the source/destination program
  couldn't be determined.
- `clipboard_events[].content` — present but `null` below
  `clipboard_content_threshold_bytes` (default 50 KB); a string truncated to
  `clipboard_content_max_bytes` (default 100 KB) at/above it.
- `sensitive[].kind` — one of `credit_card`, `sa_id_number`, `ssn`, `jwt`,
  `api_key`, `private_key`, `password`. `masked` keeps only the first/last
  2 characters.
- `changed_files` — capped at 500 entries, most recent first.
- `running_processes` — processes running at report time; CPU usage is
  computed from a double refresh (~200 ms apart). Busiest-by-CPU first,
  capped at 300 entries (the server cap).
- `folder_writes` — per-folder write counts aggregated from the full file
  scan (before the `changed_files` cap), busiest first, capped at 500
  folders. `write_count` counts files changed under the folder during the
  period; `last_write_at` is the newest change.
- All timestamps are RFC 3339 UTC (`...Z`).

## Server-side change

The minimal change is a `disks: Vec<DiskEntry>` on both payload models:

```
DiskEntry {
  mount_point: string,
  name: string,
  total_bytes: u64,
  available_bytes: u64
}
```

Make it optional/defaulted — older agents in the field don't send it yet.
