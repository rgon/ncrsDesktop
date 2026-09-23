# ncrs file-manager IPC protocol (v3)

This is the published interface between the `ncrs` service and every client:
file-manager adapters (Nautilus, Dolphin, …), `ncrs-gui` and `ncrs-ctl`. The
service owns the mount and all desktop integration. Clients never link to each
other, and nothing here requires the GUI.

## Transport

- Unix stream socket at `$XDG_RUNTIME_DIR/ncrs.sock` (mode 0600). Without
  `XDG_RUNTIME_DIR` the service uses a private per-uid directory; see
  `socket_dir()` in `ncrs_core/src/ipc.rs`.
- Request: one line, `VERB[ args]\n`, at most 16 KiB.
- Reply: exactly one line per request, except for the streaming verbs `WATCH`
  and `SUBSCRIBE`, which take over the connection.
- Separators: fields `\t`, records `\x1e` (DETAILDIR) or `\t` (change feeds).
- Paths are absolute local paths under the mount point. A path outside the
  mount gets `unknown`, an empty reply or `error: path not under mount`.
- Errors start with `error: `. An unrecognised verb replies `unknown`.
- Up to 64 concurrent connections. Adapters that load into many processes
  (KDE file dialogs) must connect lazily, only once they see a URL under the mount.

## Handshake

```
HELLO <client-id> <proto>
→ OK\t<proto>\t<package-version>\t<mount-point>\t<capabilities>
```

`client-id` is `[A-Za-z0-9._/-]{1,64}`, e.g. `nautilus`, `dolphin-kf6`,
`ncrs-gui`, `ncrs-ctl`. Adapters take the mount point from this reply rather than
parsing `config.yaml`. The capabilities field is comma-separated; current set:
`detaildir,events,watch,search,thumbnail,weburl,keep,evict,integrations`.
A protocol mismatch is logged on both sides, and the connection keeps working
for every verb both sides know.

`VERSION <n>` → `<proto>\t<package-version>` is the v2 handshake, kept as an alias.

## Status vocabulary

The machine-readable list is `status-vocabulary.txt`:

| Word | Meaning |
|---|---|
| `kept` | pinned locally ("always keep on this device") |
| `cached` | a local copy exists but may be evicted |
| `synced` | uploaded, and the local copy matches the server |
| `remote` | online only |
| `downloading` / `uploading` | a transfer is in flight |
| `pending` | written locally, upload queued (offline or retrying) |
| `partial` | directories only: some but not all children are local |
| `unknown` | outside the mount or not yet known |

`STATUS` may append `,shared`. `DETAIL`/`DETAILDIR` carry sharing in their own field.

## Queries

| Request | Reply |
|---|---|
| `STATUS <path>` | `<status>[,shared]` |
| `DETAIL <path>` | `status\tsharing\tperms\towner\tsize` |
| `DETAILDIR <dir>` | children joined by `\x1e`, each `basename\tstatus\tsharing\tperms\towner\tsize` |
| `WEBURL <path>` | Nextcloud web URL |
| `SEARCH <term>` | JSON array of result groups |
| `THUMBNAIL <path>` | `ok` or `error: …` (fetch the server preview into the freedesktop cache) |

`sharing` is `""`, `Shared by you`, `Shared with you` or `Shared`.

## Actions

The menu action ids every adapter offers, and their verbs:

| Action id | Request | Reply |
|---|---|---|
| `KEEP` | `KEEP <path>` | `ok` (asynchronous; status goes `downloading` → `kept`) |
| `EVICT` | `EVICT <path>` | `ok` |
| `WEBURL` | `WEBURL <path>` | URL (the adapter opens it) |
| `SEARCH` | `SEARCH <term>` | JSON |

Menus are built by the client from its cached status and never with a
synchronous round-trip on the UI thread.

## Change feed

The service keeps a bounded, sequence-numbered log of changes. Record syntax:

| Record | Meaning |
|---|---|
| `S:<path>` | status or metadata changed in place (re-query it) |
| `A:<path>` / `D:<path>` | file added / removed |
| `M:<path>` | file content changed |
| `DA:<path>` / `DD:<path>` | directory added / removed |
| `R:<from>\x1e<to>` | renamed |

**Polling:** `EVENTS` → `<head>`; then `EVENTS <since>` →
`<next>\t<record>\t<record>…` with at most 1000 records per reply (repeat with
`next` until a reply has none). If `since` is older than the retained log, the
reply is `<next>\tRESYNC`: drop every cached status and re-fetch.

**Push:** `WATCH [since]` → `WATCHING\t<seq>`, then `EV\t<next>\t<records…>`
(or `EV\t<next>\tRESYNC`) whenever the log moves, and `PING` after 20 s of
silence. The connection serves nothing else afterwards, so use a second
connection for queries.

**Legacy (v2):** `CHANGES` returns tab-separated status paths (at most 500) and
`FILE_CHANGES` returns the structural records above. Each keeps one cursor per
client *process*. Connections from one pid (and, after `HELLO`, the same
client-id) share it, so a multi-threaded client sees each change once. A new
process starts at "now".

## Integrations (desktop / file-browser profiles)

| Request | Reply |
|---|---|
| `INTEGRATIONS` | JSON array, one object per browser profile (see below) |
| `INTEGRATION_SET <profile> on\|off\|auto` | `ok` or `error: …` |

```json
{"id":"dolphin","name":"Dolphin (KDE)","installed":true,"mode":"auto",
 "enabled":true,"adapter_installed":false,"adapter_connected":false,
 "summary":"…","adapter_package":"ncrs-dolphin","adapter_client_ids":["dolphin-kf6","dolphin-kf5"]}
```

`mode` is the stored user choice. `auto` means enabled if and only if the browser is
installed; detection re-runs on every `INTEGRATIONS`. The profile is the only
unit a client can toggle; its components (indexer exclusion, sniff handling,
thumbnails, local-only files) are internal to the service.

## Daemon state (GUI)

`ERRORS`, `TRANSFERS`, `JOURNAL`, `CONFLICTS`, `STORAGE`, `STATE`, `PAUSE`,
`RESUME`, `PASSTHROUGH_ON|OFF|STATUS`, `PURGE_CACHE`, `SUBSCRIBE`, `CLIENTS`
(JSON list of `HELLO`ed clients), `LOG <msg>`. See `ncrs_core/src/ipc.rs`.
