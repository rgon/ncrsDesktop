# ncRS Desktop client
Performs as good in an actual business than in a recently created personal cloud

| | |
|---|---|
| ![Sync status and plugins](docs/screenshot1.jpg) | ![Settings](docs/screenshot2.jpg) |
| ![NC Metadata in Nautilus](docs/screenshot3.jpg) | ![Nextcloud-generated thumbnails even for RAW files](docs/screenshot4.jpg) |

## Feature comparison:
| Feature                   | Nextcloud Desktop | GNOME Integration/GVfs | ncRS |
| :------------------------ | :---------------- | :--------------------- | :--- |
| Real Virtual Filesystem             | ❌ (Experimental, bad approach which doesn't work with shell/file pickers etc)                | ✅ (remote only)                     | ✅   |
| Streaming Download Support (play large 4K videos at network-rate)         | ❌ | ❌ (yes, but throughput is lower) | ✅ |
| Nextcloud Notifications         | ✅ | ❌ | ✅ |
| Nautilus integration: syncing/downloaded/shared    | ✅ (partial)                | N/A since it's only online                    | ✅   |
| Instant local access      | ✅                | ❌                     | ✅   |
| Nextcloud-generated thumbnails         | ❌ | ❌ | ✅ (even for RAW images!) |
| Local cached file pruning      | ❌                | ❌                     | ✅ intelligently keeps local copies of often-used files  |
| Dynamically cache files/keep part locally | ❌                | ❌ (No internet = no files) | ✅   |
| No path conflicts | ❌ (may emit sync errors)               | ✅ | ✅ No dumb 'Some files could not be synced' |
| Maps nextcloud permissions to filesystem permissions      | ❌                | ❌ (will error, but doesn't first display it to the user)                    | ✅   |
| HPB Support/Sync speed         | ✅ NC HPB | ❌ | ✅ NC HPB |
| QUIC/HTTP3 Support         | ❌ | ❌ | ✅ |
| Syncing multiple OS paths -> different folders within Nextcloud     | ✅ | ❌ | ❌ (everything mounted under the Nextcloud path) |


## Server tips
Server-side recommendation: If you have shell access to your Nextcloud server, you shall enable background thumbnail pre-generation with occ preview:pre-generate. 

This makes the server generate thumbnails during idle time rather than on-demand, which would eliminate the congestion entirely for directories that have been indexed.

If not, disable thumbnails with the ncrs cli flag.

## Usage
Install the .deb file:
```
cd Downloads
sudo apt install ./ncrs_*_amd64.deb
```
And open it from your applications list or the terminal `ncrs-gui & disown`

### Dependencies (shall be automatically requested by the .deb)
```sh
sudo apt-get install fuse3 libfuse3-dev libxdo-dev
```

### Development dependencies
Rust: https://rustup.rs/

For the Nautilus extension (optional):
```sh
sudo apt-get install python3-nautilus
```

### First-time setup
You may directly set it up using the GUI, including 'Authorize Device'-type login from the web browser. But you may configure it -fully or partially- using the config file, which is especially useful for fleet deployment.


**Create the config file** — run the daemon once to generate the skeleton, then fill it in:
```sh
cargo run -p ncrs_core        # exits immediately, writes ~/.config/ncrs/config.yaml
nano ~/.config/ncrs/config.yaml
```

The file looks like this; use an [app password](https://docs.nextcloud.com/server/latest/user_manual/en/session_management.html#managing-devices) rather than your main password:
```yaml
url: https://cloud.example.com/remote.php/dav/files/YOUR_USERNAME/
username: youruser
password: xxxx-xxxx-xxxx-xxxx   # app password (optional) or fetched/saved from the system keyring
mount_point: /home/you/ncrs
user: youruser
```

### Provisioning (corporate / multi-user)

The `.deb` (built by `scripts/build-deb.sh` will allow fleet config. For 

- **Config is per-user** at `~/.config/ncrs/config.yaml` (XDG; there is no system-wide config). A template ships at `/usr/share/doc/ncrs/config.yaml.example`, or generate one with `ncrs --print-default-config`.
- **Push per-user config files** with your config-management tool (e.g. Ansible `template` to each user's `~/.config/ncrs/config.yaml`). You may pre-fill `/etc/skel/.config/ncrs/config.yaml` so new accounts start provisioned. If setting the password on the config file, always use per-user [app passwords](https://docs.nextcloud.com/server/latest/user_manual/en/session_management.html#managing-devices) or an `auth_command` — never a shared credential.
- **The GUI tray app autostarts at login** via `/etc/xdg/autostart/ncrs-gui.desktop` and runs the systemctl service. 
- **Headless alternative**: `systemctl --user enable --now ncrs.service` runs the daemon without the GUI. The two coexist: when the GUI starts and finds the service already serving the IPC socket, it attaches as a client — mirroring sync state, errors, and transfers in the tray and forwarding pause/resume — instead of mounting a second time. Quitting an attached tray will unmount it, however.

### Development running

**GUI + daemon** (the normal way):
```sh
./runui.sh
```
This starts the Tauri tray app; the daemon mounts WebDAV at your configured `mount_point` automatically. 

It will also install the shell integrations etc: `./shell_integration/nautilus/install.sh` to show sync-state emblems (cloud = remote-only, tick = local) on files in the mount.

Downloaded files are cached in `~/.cache/ncrs/`.

**Daemon only** (headless / for systemd):
```sh
RUST_LOG=info cargo run -p ncrs_core
```

#### Uninstall extension:
```sh
rm ~/.local/share/nautilus-python/extensions/syncstate.py
nautilus -q
```

### Running tests

Unit + integration tests (no server needed — integration tests skip gracefully):
```sh
cargo test
python3 -m unittest shell_integration.nautilus.test_syncstate -v
```

End-to-end tests against a real WebDAV server (requires Docker):
```sh
docker compose -f docker/docker-compose.yml up -d
WEBDAV_TEST_URL=http://localhost:8888 cargo test -p ncrs_core --test integration_test
docker compose -f docker/docker-compose.yml down
```

Package build + verification (what CI runs; `--container` needs Docker or Podman):
```sh
./scripts/build-deb.sh              # options: --version, --arch, --out-dir, --skip-gui, --skip-build
./scripts/test-deb.sh --container   # metadata, contents, desktop entries, clean-install smoke test
```

## TODO (development progress tracker):
```
+ [x] base tauri tray icons https://github.com/tauri-apps/tray-icon
+ [x] HTML tauri settings ui with tray icon support
	+ [x] open window only when clicking about
	+ [x] load js file, interactable (back!)
	+ [x] show icon and window separately
	+ [x] tokio setup function
	+ [x] window interactions:
		+ [x] disable close, maximize
		+ [x] hide window on close. FIX: minimize event not firing
	+ [x] app icon: gnome only loads icons from .desktop files in which case tauri dev won't be able to display the icon
	+ [x] multiple icons
	+ [x] clone all NC Desktop app options
	+ [x] save state machine
	+ [x] multiple icons given sync machine
	+ [x] pause & edit state menu
	+ [x] open in top position: cannot get this to work in Gnome. WORKED AROUND given all my target users have a single OS! -> review multi-platform support

+ [x] import fuse mount
+ [x] fix tokio error
+ [x] basic fuse mount

+ [x] send notifications multi-platform
+ [x] yaml config reader `yaml-rust2 = "0.9.0"`. Overridable with
+ [-] play notification sound: requires rodio in separate thread https://github.com/RustAudio/rodio/blob/f1eaaa4a6346933fc8a58d5fd1ace170946b3a94/examples/music_ogg.rs
+ [x] non-functional primimtive Svelte/tailwindcss UI 

+ [x] rust fuse impl?
	fuser = { version = "0.13.0", features = ["serializable"] }
	https://github.com/cberner/fuser
	or:
	https://github.com/ubnt-intrepid/polyfuse
+ [x] simple login ui with tauri, 2 crates/modules

+ [x] RELEASE:
    + [x] Systemd service
    + [x] systemd service installer
    + [x] release: snap/appimage/flatpak/what? but only in Github Actions

+ [x] Tray icon
+ [x] VFS Webdav mount
	+ [x] VFS File handling https://github.com/nextcloud/desktop/issues/3668
> FUSE is not a good solution when network is involved because the normal file API you end up using to access FUSE filesystems is not able to cope with network errors. (?)
> https://github.com/nextcloud/desktop/issues/3668#issuecomment-905330846
	that's wrong tho
	+ [x] Proper VFS with files openable through terminal

+ [x] Nextcloud notifications API
+ [x] nautilus-nextcloud icon support
	+ [x] simple, modular RPC api for multiple file browsers: fetch 'recency' of files given path or dir
	+ [x] Basic PY implementation https://linuxconfig.org/how-to-write-nautilus-extensions-with-nautilus-python
	https://gnome.pages.gitlab.gnome.org/nautilus-python/nautilus-python-migrating-to-4.html
	see: https://github.com/nextcloud/desktop/blob/master/shell_integration/nautilus/syncstate.py

+ [x] Implement files HPB push API (native Nextcloud Desktop API). Would be faster than rclone NC/WebDav [source](https://www.reddit.com/r/NextCloud/comments/ueby94/rclone_as_desktop_sync_client_replacement/)
	HPB appears to use csync https://docs.nextcloud.com/desktop/3.4/architecture.html
	'csync (this project) is a client-only file synchronizer for users using existing protocols like smb or sftp'
	-> probably smart to use remotefs-webdav and remotefs-rs, for the inbuilt SFTP support

	+ [x] HPB here: https://github.com/nextcloud/notify_push/
	It runs a websockets server on cloud.your.domain/push/ws
	There's an existing test client in rust! https://github.com/nextcloud/notify_push/blob/main/test_client/src/main.rs

+ [x] mass deployment / cli setup ensure working. Warn only apppassword https://docs.nextcloud.com/desktop/3.9/advancedusage.html#mass-deployment-and-account-creation
+ [x] HTTP/3 to avoid network overhead


+ [x] customize cache pruning frequency
+ [x] add option in our yaml to keep paths by default, without requiring the user to specify them (log an error if not found, but don't panic the application)
+ [x] allow setting the mount path with ~ and $HOME
+ [x] auto-create the mountpoint directory if it does not exist
+ [x] review: is it possible that this extension makes my nautilus hang? In which case would it cause it?

+ [x] configuration as cli flags? As yaml? review all.
+ [x] get from gnome keyring

+ [x] Feature parity with the NC browser file explorer/mobile app (share, file options, view who shared, keep remote permissions etc)

+ [x] test write to a file, check in nextcloud
    + [x] uploading icon
    + [x] keep a local copy of the file if written by us (configurable)
    + [x] can we make files within FUSE be actual filesystem/inode links, instead of passing through our rust code?
        + to actual TODO
    + [x] test locally updating a synced file
    + [x] test remotely updating a synced file

    + [x] test both to emulate a race
    + [x] test with emulate no network access, showing the local cached copy --emulate-no-network-in 15s
    + [x] ensure that the 'uploading/downloading' section in the dash app reacts to the rest

+ [x] pause sync/unmount behaviour
    + [x] impl
+ [x] CI/CD
    + [x] choose appimage/flatpak/snap -> does not play well with FUSE mount, unix socket IPC, nautilus... use apt
    + [x] Ship a .deb built with cargo-deb (for the Rust daemon

+ [x] local cache implementation:
	> + fetch file request: get etag/modification date. Do this first by folder (test!)
	> + if etag changed, fetch to nc-raw
	> + ln -s from fuse to /mount/nc-raw/
	> + save etags in DB 
	> + save file structure in db. React file structure using notify_sync (fast traversal)
	> + query db to perform cache logic and prune next file request asynchronously (prioritize latency!)
	NOTE: no need for a DB, be unix-like, make the FS be the DB

+ [x] fix `fusermount3: option allow_other only allowed if 'user_allow_other' is set in /etc/fuse.conf` without setting it - removed allow_other fuse mount option

+ [x] network error handling: EAGAIN|ETIMEDOUT https://pubs.opengroup.org/onlinepubs/009695399/functions/read.html

+ [x] share account details with gnome-online-accounts so that we don't need dual login. Get gnome-calendar working 

+ [ ] eval performant cache if needed
	with tokio_uring! https://gist.github.com/munro/14219f9a671484a8fe820eb35d26bb80
	+ https://github.com/foyer-rs/foyer
	+ https://docs.rs/freqfs/0.4.3/freqfs/
	+ https://github.com/pedrocr/syncer
	+ https://forum.autonomi.community/t/syncer-a-caching-fuse-based-filesystem-in-rust/32018
	+ https://github.com/kahing/catfs

+ [ ] E2E Encryption
+ [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache
+ [ ] cross-platform review: what do we need to change?

+ [x] UX:
    + [x] Icon mode: sync status || avatar and user status, errors
    + [x] implement 'desktop apps'?
    + [ ] Nextcloud integration with clock-in clock-out!! (DUMB spanish regulation) -> separate app? Same app that fetches conn info? Generate png icon with status?

+ [ ] QOL:
    + [x] Auto-suffix webdav://example.com/nextcloud/remote.php/dav/files/USERNAME/
	+ [x] View user login info/status: HPB Connection, DAV Connection. Turn orange if HPB NOK.
	+ [x] main Settings in tauri gui
	+ [x] Set status! Online/offline etc
    	+ [x] edit configuration/save yaml - ensure it can be provisioned
		-- cache
		+ [x] cache options: max size
		+ [x] option to pre-fetch folders up to certain size or not
                + [ ] cache algorithm: FIFO/LIFO
		+ [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache
	+ [ ] other config:
		+ [ ] play notification sound
	+ [ ] Setup flow:
		+ config exists? -> load: ok|err ->
		+ ask for login flow in browser: https://github.com/traxys/nextcloud-passwords-client
		+ save as yaml, lock yaml file permissions

+ [ ] merge login with authd login if machines use nc-idam-provided OAuth2 token

+ [ ] missing test surface:
    + [ ] diffing algorithm? -> ask which copy we want to save/save conflicting copy separately -> choose conflict resolution strategy
    + [ ] test moving -> ensure it's a move/rename operation and not just a delete/copy

+ [ ] manually review tests

```



## Future
+ [ ] Clean branding images/icons etc. Ask NC team
+ [ ] Add project to remotefs-rs/remotefs-rs list of used projects

## Cache and Storage Paths

ncrs stores locally-available files under `~/.cache/ncrs/<server-hash>/`:

| Directory | Purpose | Survives restart | User-controlled |
|-----------|---------|-----------------|-----------------|
| `kept/`   | Files explicitly pinned via "Keep Locally" | Yes | Yes (KEEP/EVICT) |
| `cache/`  | Files auto-downloaded during reads | Yes (validated on boot) | No (may be evicted) |

The `<server-hash>` component is derived from the WebDAV URL to allow multiple server configurations.

Additional metadata files in the root:
- `dir_cache.json` -- cached directory listings (etags + entries)
- `file_cache.json` -- tracks which files are cached/kept locally (remote path, etag, kept flag)
- `journal.bin` -- offline mutation journal for pending uploads/deletes/renames

Config options (in `~/.config/ncrs/config.yaml`):
- `auto_keep_locally_modified_files: true` -- keep a local copy after uploading a file you edited
- `auto_keep_cached_files: true` -- promote read-cached files to kept automatically
- `cache_max_size_bytes: 34359738368` -- max size for `cache/` directory (default 32 GB, 0 = unlimited). Oldest-accessed files are evicted first.
- `cache_auto_purge_days: 10` -- auto-delete cached files not accessed in N days (default 10, 0 = disabled)
- `cache_streamed_reads: true` -- promote fully-streamed files to disk cache
- `read_ahead_bytes: 67108864` -- read-ahead window for streaming reads (default 64 MB)
- `cache_cleanup_interval_secs: 3600` -- how often to run cache pruning (default 3600 = 1 hour)
- `keep_paths: ["/Documents", "/Photos"]` -- remote paths to auto-keep locally on startup (default empty)
