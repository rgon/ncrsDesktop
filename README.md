Dependencies:
	sudo apt-get install fuse3 libfuse3-dev libxdo-dev
	rustup

### TODO (minimal):
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
	+ [ ] open in top position

+ [x] import fuse mount
+ [x] fix tokio error
+ [x] basic fuse mount

+ [ ] send notifications multi-platform

+ [ ] yaml config reader `yaml-rust2 = "0.9.0"`. Overridable with 

+ [ ] Webdav implementation
+ [ ] webdav://example.com/nextcloud/remote.php/dav/files/USERNAME/
+ [ ] Proper login flow as here: https://github.com/traxys/nextcloud-passwords-client
+ [ ] Actual tauri UI

### Functionality
+ [x] Tray icon
+ [ ] VFS File handling
+ [ ] VFS Webdav mount
	+ [ ] Proper VFS with files openable through terminal
+ [ ] Nextcloud notifications
+ [ ] nautilus-nextcloud icon support
	+ [ ] simple, modular RPC api for multiple file browsers: fetch 'recency' of files given path or dir
	+ [ ] Basic PY implementation https://linuxconfig.org/how-to-write-nautilus-extensions-with-nautilus-python
	https://gnome.pages.gitlab.gnome.org/nautilus-python/nautilus-python-migrating-to-4.html

+ [ ] Implement files HPB push API (native Nextcloud Desktop API). Would be faster than rclone NC/WebDav [source](https://www.reddit.com/r/NextCloud/comments/ueby94/rclone_as_desktop_sync_client_replacement/)
	+ [ ] support multiple backends: webdav, HPB...
+ [ ] mass deployment / cli setup ensure working. Warn only apppassword https://docs.nextcloud.com/desktop/3.9/advancedusage.html#mass-deployment-and-account-creation

+ [ ] local cache implementation
+ [ ] E2E Encryption
+ [ ] ignored files regex (filter from list, filter from sync) -> keep only in cache
> This vs GVfs
+ Webdav is super slow (~5/10s delay)
+ Potential for local cache

### Known bugs
+ [ ] fix `fusermount3: option allow_other only allowed if 'user_allow_other' is set in /etc/fuse.conf` without setting it
