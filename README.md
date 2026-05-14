# ncRS Desktop client

![Main dialog](docs/screenshot.png)
FIX:
+ [x] proper & performant-enough read
+ [x] re-starting ncrs story

+ [ ] emulate no network access, showing the local cached copy --emulate-no-network-in 15s

+ [ ] writes:
    + [ ] basic file write: local cache-public upload?
    + [ ] working without network
    + [ ] diffing algorithm? -> ask which copy we want to save/save conflicting copy separately -> choose conflict resolution strategy
```
GOALS:
+ [x] Real vFS on linux: not downloading everything then serving it. Uses the built-in nextcloud thumbnailer for a rich experience
+ [ ] webdav with QUICK/HTTP3 support for speed
+ [ ] Feature parity with the NC file explorer (share, file options, view who shared, keep remote permissions etc)
    + add/remove from favorites
    + details
    + rename/move or copy
    + send/share (same as details view)
    + 'sync'
+ [ ] Can we hook into the search function (in FUSE or nautilus) to use the built-in nextcloud search?

+ [ ] No dumb 'Some files could not be synced' - Filename contains trailing spaces -> saving will immediately cause a filesystem error
+ [ ] Dash app has feature parity with the Android/iOS app (in driver)
+ [ ] Performs as good in a heavy enterprise than in a new personal cloud
+ [ ] Enterprise OAuth2 login with authd-shared token for automatic login for multi-user computers and zero touch provisioning to new machines
+ [ ] Abstract file explorer API implementation - concrete cosumers (nautilus, demo web GUI, etc)
```

## Feature Goals:
| Feature                   | Nextcloud Desktop | GNOME Integration/GVfs | ncRS |
| :------------------------ | :---------------- | :--------------------- | :--- |
| Nextcloud Notifications         | ✅ | ❌ | ✅ |
| Full Shell integration    | ✅                | N/A since it's only online                    | ✅   |
| Instant local access      | ✅                | ❌                     | ✅   |
| Virtual Files             | ❌ (Experimental, bad approach which doesn't work with shell/file pickers etc)                | ✅ (remote only)                     | ✅   |
| Dynamically cache files/keep part locally | ❌                | ❌ (No internet = no files) | ✅   |
| HPB Support/Speed         | ❓ (Not explicitly stated, generally good sync performance) | ❌ | ✅ |

## Usage

### Dependencies

```sh
sudo apt-get install fuse3 libfuse3-dev libxdo-dev
```
Rust: https://rustup.rs/

For the Nautilus extension (optional):
```sh
sudo apt-get install python3-nautilus
```

### First-time setup

**1. Create the mount point** — any empty directory works:
```sh
mkdir -p ~/ncrs
```

**2. Create the config file** — run the daemon once to generate the skeleton, then fill it in:
```sh
cargo run -p ncrs_core        # exits immediately, writes ~/.config/ncrs/config.yaml
$EDITOR ~/.config/ncrs/config.yaml
```

The file looks like this; use an [app password](https://docs.nextcloud.com/server/latest/user_manual/en/session_management.html#managing-devices) rather than your main password:
```yaml
url: https://cloud.example.com/remote.php/dav/files/YOUR_USERNAME/
username: youruser
password: xxxx-xxxx-xxxx-xxxx   # app password
mount_point: /home/you/ncrs
user: youruser
```

### Running

**GUI + daemon** (the normal way):
```sh
./runui.sh
```
This starts the Tauri tray app; the daemon mounts WebDAV at your configured `mount_point` automatically. Downloaded files are cached in `~/.cache/ncrs/`.

**Daemon only** (headless / for systemd):
```sh
RUST_LOG=info cargo run -p ncrs_core
```

### Nautilus integration

Install the shell extension to show sync-state emblems (cloud = remote-only, tick = local) on files in the mount:
```sh
./shell_integration/nautilus/install.sh
nautilus -q   # restart Nautilus
```

Uninstall:
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

## TODO (development progress tracker):
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
	+ [ ] open in top position: cannot get this to work in Gnome. WORKED AROUND given all my target users have a single OS! -> review multi-platform support

+ [x] import fuse mount
+ [x] fix tokio error
+ [x] basic fuse mount

+ [x] send notifications multi-platform
+ [x] yaml config reader `yaml-rust2 = "0.9.0"`. Overridable with
+ [-] play notification sound: requires rodio in separate thread https://github.com/RustAudio/rodio/blob/f1eaaa4a6346933fc8a58d5fd1ace170946b3a94/examples/music_ogg.rs
+ [x] non-functional primimtive Svelte/tailwindcss UI 

----

+ [ ] rust fuse mvp?
	fuser = { version = "0.13.0", features = ["serializable"] }
	https://github.com/cberner/fuser
	or:
	https://github.com/ubnt-intrepid/polyfuse
	
	STEPS:
	+ [ ] create fuse with 4 folders that say /fuse/burns/pyro/spark
	+ [ ] login via webdav
	+ [ ] file directory map with fuse
	+ [ ] fetch files with fuse
	+ [ ] when fetching, check version number some way
	+ [ ] simple login ui with tauri, 2 crates/modules
	----- LEAVE HERE
+ [ ]https://xethub.com/blog/nfs-fuse-why-we-built-nfs-server-rust 
----

+ [ ] Webdav implementation
+ [ ] Auto-suffix webdav://example.com/nextcloud/remote.php/dav/files/USERNAME/

+ [ ] Functional tauri menu UI:
	+ [x] get avatar: https://github.com/nextcloud/desktop/blob/cd44540a5a30c1e639edc8082228d211a3d9a34b/src/libsync/networkjobs.cpp#L787C1-L787C148
		yourcloud.domain/remote.php/dav/avatars/userID/256.png
	+ [ ] View user login info/status: HPB Connection, DAV Connection. Turn orange if HPB NOK.
	+ [ ] Access mounted folder
	+ [ ] main Settings:
		+ [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache
		-- cache
		+ [ ] cache options: max size, algorithm: FIFO/LIFO
		+ [ ] option to pre-fetch folders up to certain size or not
	+ [ ] other config:
		+ [ ] play notification sound
		+ [ ] show sync notifications
		+ [ ] ?
	+ [ ] Setup flow:
		+ config exists? -> load: ok|err ->
		+ ask for login flow in browser: https://github.com/traxys/nextcloud-passwords-client
		+ save as yaml, lock yaml file permissions
	+ [ ] Set status! Online/offline etc
	----
	WEB API:
	+ [ ] fetch notifications
	+ [ ] edit configuration/save yaml
	+ [ ] open local folder

+ [ ] network error handling: EAGAIN|ETIMEDOUT https://pubs.opengroup.org/onlinepubs/009695399/functions/read.html
+ [ ] Systemd service
+ [ ] systemd service installer
+ [ ] release: snap/appimage/flatpak/what? but only in Github Actions
+ [ ] Icon mode: sync status || avatar and user status, errors
+ [ ] Nextcloud integration with clock-in clock-out!! (DUMB spanish regulation) -> separate app? Same app that fetches conn info? Generate png icon with status?

### Functionality/Service TODO
+ [x] Tray icon
+ [ ] VFS File handling https://github.com/nextcloud/desktop/issues/3668
> FUSE is not a good solution when network is involved because the normal file API you end up using to access FUSE filesystems is not able to cope with network errors. (?)
> https://github.com/nextcloud/desktop/issues/3668#issuecomment-905330846
+ [ ] VFS Webdav mount
	+ [ ] Proper VFS with files openable through terminal

+ [ ] Nextcloud notifications API
+ [ ] nautilus-nextcloud icon support
	+ [ ] simple, modular RPC api for multiple file browsers: fetch 'recency' of files given path or dir
	+ [ ] Basic PY implementation https://linuxconfig.org/how-to-write-nautilus-extensions-with-nautilus-python
	https://gnome.pages.gitlab.gnome.org/nautilus-python/nautilus-python-migrating-to-4.html
	see: https://github.com/nextcloud/desktop/blob/master/shell_integration/nautilus/syncstate.py

+ [ ] Implement files HPB push API (native Nextcloud Desktop API). Would be faster than rclone NC/WebDav [source](https://www.reddit.com/r/NextCloud/comments/ueby94/rclone_as_desktop_sync_client_replacement/)
	+ [ ] support multiple backends: webdav, HPB...
	HPB appears to use csync https://docs.nextcloud.com/desktop/3.4/architecture.html
	'csync (this project) is a client-only file synchronizer for users using existing protocols like smb or sftp'
	-> probably smart to use remotefs-webdav and remotefs-rs, for the inbuilt SFTP support

	+ [ ] HPB here: https://github.com/nextcloud/notify_push/
	It runs a websockets server on cloud.your.domain/push/ws
	There's an existing test client in rust! https://github.com/nextcloud/notify_push/blob/main/test_client/src/main.rs

+ Can we implement webdav and HPB? -> yes it does seem like it!

+ [ ] mass deployment / cli setup ensure working. Warn only apppassword https://docs.nextcloud.com/desktop/3.9/advancedusage.html#mass-deployment-and-account-creation

+ [ ] local cache implementation:
> + fetch file request: get etag/modification date. Do this first by folder (test!)
> + if etag changed, fetch to nc-raw
> + ln -s from fuse to /mount/nc-raw/
> + save etags in DB 
> + save file structure in db. React file structure using notify_sync (fast traversal)
> + query db to perform cache logic and prune next file request asynchronously (prioritize latency!)

	with tokio_uring! https://gist.github.com/munro/14219f9a671484a8fe820eb35d26bb80
	+ https://github.com/foyer-rs/foyer
	+ https://docs.rs/freqfs/0.4.3/freqfs/
	+ https://github.com/pedrocr/syncer
	+ https://forum.autonomi.community/t/syncer-a-caching-fuse-based-filesystem-in-rust/32018
	+ https://github.com/kahing/catfs
	TDD this!
Choose DB:
https://github.com/cberner/redb
https://github.com/rusqlite/rusqlite
Turso Limbo

+ [ ] E2E Encryption
+ [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache
> This vs GVfs
+ Webdav is super slow (~5/10s delay)
+ Potential for local cache

### Known bugs
+ [ ] fix `fusermount3: option allow_other only allowed if 'user_allow_other' is set in /etc/fuse.conf` without setting it


## Future
+ [ ] Clean branding images/icons etc. Ask NC team
+ [ ] Add project to remotefs-rs/remotefs-rs list of used projects
