
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

+ [x] set a Content-Security-Policy in tauri.conf.json (was `"csp": null`)
	> Verified headless: the built frontend loads with 0 violations under the new
	> policy, and a deliberately strict control (`script-src 'self'`) produces 3
	> violations and a dead 1.7 KB DOM — so the two `'unsafe-inline'` keywords are
	> required, not lazy. Tauri only nonces `script[src^='http']` and `<style>`
	> elements, never inline scripts, and app.html + SvelteKit ship two inline
	> scripts plus a `style="display: contents"` attribute.
	> `img-src` is `'self' data:` — no remote origin at all. That is only possible
	> because the daemon inlines server icons as data: URIs
	> (`asset_url::inline_asset`) instead of letting the webview fetch them, so
	> the policy needs no user-configured origin and fails closed. Verified: a
	> remote <img> is blocked, a data: <img> loads.

+ [x] deploy the dir-cache memory fix — a running 0.1.56 still idles at ~690 MB
+ [x] UX:
    + [x] Icon mode: sync status || avatar and user status, errors
    + [x] implement 'desktop apps'?
    + [ ] Nextcloud integration with clock-in clock-out -> separate app? Same app that fetches conn info? Generate png icon with status?

+ [x] implement FUSE Passthrough for cache
+ [x] QOL 1
    + [x] Auto-suffix webdav://example.com/nextcloud/remote.php/dav/files/USERNAME/
	+ [x] View user login info/status: HPB Connection, DAV Connection. Turn orange if HPB NOK.
	+ [x] main Settings in tauri gui
	+ [x] Set status! Online/offline etc
    	+ [x] edit configuration/save yaml - ensure it can be provisioned
		-- cache
		+ [x] cache options: max size
		+ [x] option to pre-fetch folders up to certain size or not
	+ [x] Setup flow:
		+ config exists? -> load: ok|err ->
		+ ask for login flow in browser: https://github.com/traxys/nextcloud-passwords-client
		+ save as yaml, lock yaml file permissions


+ [ ] login keyring for authd-backed credentials: so users may log in with OAuth onto ubuntu and then ncrsDesktop will auto-login to that nextcloud account with the keyring-saved creds (without kerberos nor anything more complex), but that's a specific use case for us :)
+ [ ] cross-platform review: what do we need to change?

+ [ ] To review:
    + [ ] E2E Encryption
    + [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache

    + [ ] cache-merge algorithm: FIFO/LIFO
    + [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache
	+ [ ] other config:
		+ [ ] play notification sound

    + [ ] diffing algorithm? -> ask which copy we want to save/save conflicting copy separately -> choose conflict resolution strategy
    + [ ] test moving -> ensure it's a move/rename operation and not just a delete/copy

+ [ ] Clean branding images/icons etc. Ask NC team

