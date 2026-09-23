# Plan: modular file-browser / desktop integration (Dolphin first)

Status: implemented on branch `docs/desktop-integration-plan` (Phases 1–7, KDE items
pending measurement). Based on `master` @ `a190f75`. Line references point at that commit.

### Deviations from the plan as implemented

- Profile choices and applied side effects live in the service-owned
  `~/.config/ncrs/desktop-profiles.json`, not `config.yaml`: the GUI rewrites
  config.yaml wholesale and would clobber them.
- The Baloo exclusion is removed when the Dolphin profile is disabled, not on every
  unmount. Otherwise Baloo could start crawling in the window between mount and
  re-adding it.
- Legacy `CHANGES`/`FILE_CHANGES` cursors are keyed by client **pid**, not by pid plus
  client-id, because Nautilus says hello on only one of its pool-thread
  connections. Cursors are created only for clients that poll.
- Phase 0 (measurement on a KDE session) was not run. The Qt sniff probe, KIO `.part`
  handling and folder-view metadata are documented as open in GOTCHAS.md §4 rather
  than implemented blind.
- **One `.deb`, not per-adapter packages.** `ncrs` ships the Nautilus extension and
  the Dolphin plugin for both KF5 and KF6 (they install to different Qt plugin
  dirs). The plugin's Qt/KF libraries stay out of `Depends`, since only Dolphin
  loads it and Dolphin brings them. CI builds KF5 on the jammy runner and KF6 in
  `debian:trixie`. The ServiceMenu goes to `kio/servicemenus` for both, which
  KF5 >= 5.85 also reads. With no separate package there is no "install the
  adapter" hint in the GUI.

## Goal

A KDE user gets sync emblems and Keep / Evict / Open-in-web actions in Dolphin.
Browsing the mount must never cause downloads (no indexer, thumbnail or MIME-sniff
storms). Adding another browser (Nemo, Caja, Thunar, …) should be one "support X"
module plus its shell adapter, with no new special cases in the FUSE layer.

## Architectural rules

1. **The `ncrs` service owns everything.** It builds and serves the mounts and owns
   all file-browser and desktop logic: detection, policies and side effects such as
   the indexer exclusion.
2. **The GUI and the shell plugins are peer IPC clients.** `ncrs-gui`, the Nautilus
   extension, the Dolphin plugin and `ncrs ctl` all talk to the service over
   `$XDG_RUNTIME_DIR/ncrs.sock`. Nothing links to, depends on or requires the GUI.
   A headless or systemd install gets full integration.
   (On master the GUI already spawns or attaches to a separate `ncrs` daemon:
   `ncrs-gui/src-tauri/src/lib.rs:1385-1420`.)
3. **Packages:** the adapters never depend on `ncrs-gui`. (Planned as separate
   `ncrs-nautilus` / `ncrs-dolphin` packages; shipped inside the one `ncrs` .deb,
   see Deviations.)
4. **The GUI packs integrations per browser profile.** The user sees one
   enable/disable toggle per browser (Nautilus, Dolphin, …) and never the individual
   components (GIO sniff, Tracker ignore, …).

## Profile model: `ncrs_core/src/desktop/`

```
desktop/
  mod.rs          DesktopPolicy: merges enabled profiles; FUSE hooks ask it
  detect.rs       is-installed probes (binary on PATH, .desktop file, dpkg); resolves `auto`
  indexer/        tracker.rs  (.trackerignore)
                  baloo.rs    (balooctl6 / baloofilerc excludeFolders)
  toolkit/        gio.rs      (O_NOATIME sniff, GLib thumbnail URIs, .goutputstream-/.xdp-)
                  kio.rs      (Qt sniff pattern, .part/.kate-swp, large/x-large thumbnails)
  browser/        nautilus.rs = gio + tracker + adapter descriptor
                  dolphin.rs  = kio + baloo   + adapter descriptor
                  nemo.rs     = gio + tracker (later)
```

What each area decides:

| Area | Trait | GNOME (existing code, moved) | KDE (new, measured in Phase 0) |
|---|---|---|---|
| Indexer exclusion | `IndexerPolicy` | synthetic `.trackerignore` (`lib.rs:7267`, `:1136`) | Baloo `excludeFolders`: added when active, removed on disable or unmount |
| Sniff probe | `SniffPolicy` | `O_NOATIME` read of 32 KiB or less, answered with magic bytes from cached content-type (`lib.rs:4291`, `:730-900`) | Qt `QMimeDatabase` read pattern (not yet known) |
| Thumbnails | `ThumbnailPolicy` | `normal` only, GLib URI key, touch-on-setattr (`preview.rs:11,164-184,357-380`) | also `large`/`x-large`; `QUrl` encoding parity tests |
| Folder-view metadata | `LocalOnlyPolicy` | — | `user.kde.fm.viewproperties` xattr, `.directory`: kept local, never uploaded |
| Temp/junk files | `LocalOnlyPolicy` | `is_gio_temp_file` (`lib.rs:1480`), `is_lock_junk` (`:7286`) | KIO `.part`, `.kate-swp` |
| Shell adapter | `AdapterDescriptor` | nautilus-python extension, `nautilus -q` | KF5/KF6 overlay plugin + ServiceMenu |

**Composition.** Profiles are the only unit the user enables or disables. Components
are ref-counted: a component stays active while at least one enabled profile uses it
(for example `gio` stays on while either Nautilus or Nemo is enabled). The FUSE hot
paths query `DesktopPolicy`, an immutable snapshot swapped with `ArcSwap` on
toggle, so they take no locks.

**Defaults come from installation detection.** Every profile starts in `auto`, and
`auto` means *enabled if and only if that browser is installed*. Each
`browser/<x>.rs` declares how to detect its browser, for example binaries on
`$PATH` (`nautilus`, `dolphin`, `nemo`), their `.desktop` files under
`XDG_DATA_DIRS`, or the dpkg package. The running desktop session
(`XDG_CURRENT_DESKTOP`) is **not** the criterion: a GNOME user with Dolphin
installed gets Dolphin support too, because Dolphin and Baloo can still reach the
mount.

Detection runs at service start and again whenever the GUI or `ncrs ctl` queries
`INTEGRATIONS`. That way installing or removing a browser changes the default
without a service restart. An explicit user choice (`on`/`off`, set through
`INTEGRATION_SET` or by hand in
`config.yaml` as `desktop_profiles: { dolphin: on|off|auto }`) is persisted and
overrides detection until reset to `auto`. Indexer exclusions
apply whenever the profile is active, even if its shell adapter package is not
installed, because Baloo crawling the mount downloads everything regardless.

## IPC protocol v3 (additive; v2 clients keep working)

The spec moves to `shell_integration/file-managers/PROTOCOL.md`, with a
machine-readable `status-vocabulary.txt` that the Rust, Python and C++ tests all
check.

- `HELLO <client-id> <proto>` → `OK\t<proto>\t<pkg-ver>\t<mount>\t<caps>`.
  Adapters stop parsing `config.yaml` (`syncstate.py:96`). `VERSION` stays as an alias.
- `CLIENTS`: list connected adapters by client-id. This replaces the hard-coded
  "Nautilus" in `ipc.rs:1092-1106`.
- **Change log (blocker fix).** Today `CHANGES`/`FILE_CHANGES` (`ipc.rs:903,931`)
  *drain* one global queue, so only one client ever sees each change. Replace it
  with a bounded ring of `(seq, kind, path)`:
  - `EVENTS <since>` → `<next-seq>\t<records>`, or `RESYNC` for a client that fell
    off the ring.
  - `WATCH` push stream (`EV …` / `PING`), reusing the `StatePush` pattern
    (`ipc.rs:408`).
  - The legacy commands become per-connection cursors over the ring.
- `INTEGRATIONS` → one record per **profile**: `id, name, installed, mode(auto|on|off),
  effective_enabled, adapter_installed, adapter_connected`. The service re-runs
  detection before it replies.
- `INTEGRATION_SET <profile> on|off|auto`: persists to config.yaml and swaps the policy.
- Menus stay client-static (built synchronously on the UI thread); the spec fixes
  the action ids `KEEP`, `EVICT`, `WEBURL`, `SEARCH`.

`ncrs ctl keep|evict|weburl|status|integrations|integration-set …` is a thin CLI
over the same socket. It backs Dolphin ServiceMenus, Nemo actions, Thunar `uca.xml`
and scripting.

## GUI (IPC-only)

The Settings section is called "File browsers". It shows one card per profile:
name, an installed/connected badge, a one-line summary of what it covers (for example
"Hides the mount from Baloo, prevents thumbnail and type-detection downloads, sync
emblems"), and a **single toggle**. The toggle's default position is whatever
detection resolved: on for browsers that are installed, off for those that aren't.
Once the user flips it, it shows as a manual override with a "reset to automatic"
action. If the adapter package is missing, it shows an
`apt install ncrs-dolphin` hint and a "Restart Dolphin" button. The GUI never shows
components and never touches Baloo or Tracker directly.

## Layout and packaging

- `shell_integration/file-managers/{nautilus,dolphin,…}/`, created with `git mv` so
  history is kept. Split `syncstate.py` into `ncrs_fmclient.py` (socket, DETAILDIR
  cache, events) plus a thin provider shim. Nemo and Caja reuse it.
- Dolphin: CMake + ECM with a `-DQT_MAJOR_VERSION=5|6` switch.
  - `KOverlayIconPlugin`: synchronous `getOverlays` from a cache, `overlaysChanged`
    driven by `WATCH`.
  - A shared `NcrsClient` singleton that connects lazily, only once a URL under the
    mount is seen.
  - A ServiceMenu `.desktop` that calls `ncrs ctl`.
- `.deb` split:
  - `ncrs` drops the `python3-nautilus` Depends (`build-deb.sh:122`).
  - `ncrs-nautilus` (arch all) takes the extension and the `nautilus -q` postinst
    block.
  - `ncrs-dolphin` (arch-specific, `dpkg-shlibdeps`) is built per distro series:
    KF5 for noble, KF6 for trixie/plucky+.
  - Add `Replaces/Breaks` for the moved files. `ncrs` Recommends `ncrs-nautilus`
    for one release.

## Phases (small commits)

0. **Measure.** On a KDE VM against a test mount (never the live `~/Nextcloud`),
   strace and FUSE-debug Dolphin browsing, Baloo, KIO thumbnails at 1x/2x, and
   folder-view writes. Record the results in `GOTCHAS.md`. This fills in the KDE
   column of the table above.
1. **Core IPC.** `PROTOCOL.md` + vocabulary test; change ring + `EVENTS` +
   per-connection legacy cursors; `HELLO`/`CLIENTS`; `WATCH`; `ncrs ctl`.
2. **`desktop/` refactor with no behaviour change.** Move the GLib/Tracker code into
   `toolkit/gio.rs` and `indexer/tracker.rs` behind the traits, and add the
   `nautilus` profile. The existing unit tests and the e2e MIME/thumbnail scenarios
   stay green.
3. **Profiles over IPC.** `INTEGRATIONS` / `INTEGRATION_SET`, config persistence,
   ref-counted component activation.
4. **Nautilus move + packaging split.** New paths, the adapter sends `HELLO` and
   consumes `EVENTS`, `ncrs-nautilus` package, updated `test-deb.sh`.
5. **KDE components.** `toolkit/kio.rs`, `indexer/baloo.rs` and the `dolphin`
   profile, driven by the Phase 0 findings.
6. **Dolphin adapter.** Overlay plugin, ServiceMenu, QtTest against a fake
   `QLocalServer`, `ncrs-dolphin` package, CI on `debian:trixie` (KF6) and
   `ubuntu:24.04` (KF5).
7. **GUI "File browsers" section** (per-profile toggles over IPC).
8. **Later:** `KAbstractFileItemActionPlugin` for status-aware menus, Nemo and Caja
   profiles, Thunar `uca.xml`.

## Tests

- **Rust:** ring/cursor semantics (two clients both see a change; overflow gives
  RESYNC), HELLO parsing, vocabulary match, v2 compatibility, profile ref-counting
  (disabling Nautilus keeps `gio` active while Nemo is on), policy snapshot swap,
  `auto` resolution against a fake PATH/XDG_DATA_DIRS (installed → on, absent →
  off, explicit override wins, reset → back to detection).
- **Python:** the existing `test_syncstate.py` plus `ncrs_fmclient` event tests.
- **C++:** QtTest parsing, cache and `overlaysChanged` tests.
- **Docker e2e:** two concurrent `EVENTS` clients; `ncrs ctl` round-trips; a
  Qt-style probe scenario once Phase 0 confirms the pattern; a Baloo exclusion
  add/remove scenario.

## Risks and open questions

- **Connection count.** KDE file dialogs in every app load the overlay plugin.
  `MAX_IPC_CLIENTS = 64` (`ipc.rs:36`) with one thread per client, so the plugin
  must connect lazily.
- **Qt sniff detection.** A second probe signal may only be distinguishable by
  read size and offset, which risks serving fake bytes to real readers (precedent:
  the `cp` false positives at `lib.rs:779-806`).
- **Baloo exclusion ownership.** Only remove entries the service added (record
  them), so user-set exclusions are never touched.
- **Unverified assumptions.** Does KIO treat `fuse.ncrs` as a remote filesystem for
  previews, and would setting `MountOption::Subtype` (`lib.rs:6606`) change that?
- **Build matrix.** KF5/KF6 per-series builds; CI time; root disk space.
