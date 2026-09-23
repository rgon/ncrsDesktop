# ncrs for Dolphin (KDE)

Sync-status emblems and context-menu actions for files under the ncrs
Nextcloud mount in Dolphin. Two pieces:

- `ncrsoverlayplugin.so`, a `KOverlayIconPlugin` that Dolphin loads from
  `<qt-plugins>/kf6/overlayicon/` (or `kf5/overlayicon/`). It speaks the ncrs
  IPC protocol v3 (`../PROTOCOL.md`) as client `dolphin-kf6` / `dolphin-kf5`.
- `servicemenu/ncrs.desktop`, a ServiceMenu with a "Nextcloud (ncrs)" submenu:
  *Always Keep on This Device* (`ncrs-ctl keep`), *Free Up Space*
  (`ncrs-ctl evict`) and *Open in Nextcloud Web* (`ncrs-ctl weburl --open`).
  ServiceMenus cannot be limited to one directory, so the submenu shows for
  every local file; `ncrs-ctl` refuses paths outside the mount.

`ncrs` detects the adapter by the plugin file and shows it as connected once
the plugin has said `HELLO` (`ncrs-ctl clients`).

## Emblems

The mapping lives in one table, `src/ncrsemblems.cpp`, and the tests check that
it covers every word in `../status-vocabulary.txt` and that each icon exists in
Breeze at 8/16/22 px (the sizes Breeze 5 and 6 both ship).

| Status | Emblem | |
|---|---|---|
| `kept` | `emblem-checked` | green square with a tick |
| `cached` | `vcs-normal` | green circle with a tick |
| `downloading`, `uploading`, `pending` | `vcs-update-required` | amber circle with arrows |
| `partial` (directories) | `emblem-information` | blue |
| `synced`, `remote`, `unknown` | none | |
| any sharing | `emblem-shared` | added after the status emblem |

Breeze ships no `emblem-default`, `emblem-downloads` or full-colour
`emblem-synchronizing` (the names the Nautilus adapter uses under Adwaita), so
the table uses the names Dolphin's own VCS plugins use.

## How it behaves

- Dolphin calls `getOverlays()` on the UI thread for every visible item. The
  plugin answers from a per-directory `DETAILDIR` cache (30 s TTL) and never
  waits on the socket. On a miss it returns nothing, fetches the parent
  directory asynchronously and emits `overlaysChanged()` for the items whose
  emblems changed.
- The plugin also loads into processes that never touch the mount. It connects
  on the first local-file lookup, since the mount point only arrives with the
  `HELLO` reply. If the socket file is missing it retries with backoff
  (1 s up to 60 s) and only while a lookup is waiting.
- A second connection runs `WATCH` once a directory under the mount is listed.
  `S:`/`A:`/`D:`/`M:`/`DA:`/`DD:`/`R:` records mark the parent listing (and
  any cached ancestors, for `partial`) stale and refetch it. Bursts are
  coalesced, and `RESYNC` refetches every cached directory. After a reconnect
  the stream resumes with `WATCH <seq>`.

## Build

Needs CMake, a C++17 compiler, extra-cmake-modules and the Qt/KF development
packages.

```sh
# Qt6 / KF6 (Debian trixie, Ubuntu 25.04+, Fedora, Arch)
sudo apt install cmake g++ extra-cmake-modules qt6-base-dev libkf6kio-dev libkf6coreaddons-dev
cmake -S . -B build -DCMAKE_INSTALL_PREFIX=/usr
cmake --build build
(cd build && QT_QPA_PLATFORM=offscreen ctest --output-on-failure)

# Qt5 / KF5 (Ubuntu 24.04, Debian bookworm)
sudo apt install cmake g++ extra-cmake-modules qtbase5-dev libkf5kio-dev libkf5coreaddons-dev
cmake -S . -B build5 -DCMAKE_INSTALL_PREFIX=/usr -DQT_MAJOR_VERSION=5
cmake --build build5
```

`-DBUILD_TESTING=OFF` skips the tests (they need the Qt Test module, which is
part of `qt6-base-dev` / `qtbase5-dev`).

## Install

```sh
sudo cmake --install build
kquitapp6 dolphin    # kquitapp5 on Plasma 5; Dolphin reads plugins at startup
dolphin &
```

This installs:

| File | KF6 | KF5 |
|---|---|---|
| plugin | `/usr/lib/<triplet>/qt6/plugins/kf6/overlayicon/ncrsoverlayplugin.so` | `/usr/lib/<triplet>/qt5/plugins/kf5/overlayicon/ncrsoverlayplugin.so` |
| ServiceMenu | `/usr/share/kio/servicemenus/ncrs.desktop` | same (read by KF5 >= 5.85) |

The `ncrs` .deb ships both plugins. To package it locally, stage them first:
`scripts/build-dolphin-plugin.sh` (add `--kf5` for Plasma 5) installs into
`dist/dolphin/kf6` (or `kf5`), which `scripts/build-deb.sh` picks up.

## Debug

```sh
QT_LOGGING_RULES="ncrs.dolphin.debug=true" dolphin 2>&1 | grep ncrs
ncrs-ctl clients            # is dolphin-kf6 connected?
ncrs-ctl status ~/Nextcloud/some/file
```

- No emblems at all: check the plugin is in the directory Dolphin searches
  (`qtpaths6 --plugin-dir`, or `QT_PLUGIN_PATH`), then restart Dolphin.
  `QT_DEBUG_PLUGINS=1 dolphin` shows every plugin it tries to load.
- Emblems missing but connected: Dolphin only draws icons the current theme
  has. The names above are Breeze names; other themes may lack some.
- Stale emblems: listings refresh on `WATCH` records, and otherwise when an
  item is shown again more than 30 s after its directory was fetched.
