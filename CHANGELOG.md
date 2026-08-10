# Changelog

## [0.1.51](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.50...ncrs-v0.1.51) (2026-08-10)


### Features

* --auto-keep-locally-modified-files flag controls post-upload emblem and local copy ([ef4fba1](https://github.com/rgon/ncrsDesktop/commit/ef4fba1b123b0ad3faa9daaec27ced24a0c56936))
* add 'View in Nextcloud Web' right-click menu via WEBURL IPC command ([12b64cf](https://github.com/rgon/ncrsDesktop/commit/12b64cfd0b9af16a25a196510adcaced66552d5b))
* add bearer_token and auth_command to config ([9cbbc23](https://github.com/rgon/ncrsDesktop/commit/9cbbc2394b0cbf3c92a9d8075158853141513c22))
* add cache cleanup with configurable max size and auto-purge by age ([07b3e8e](https://github.com/rgon/ncrsDesktop/commit/07b3e8e62b6ae4e923abc279a8875f9d7044b503))
* add CHANGES invalidation tracing to Nautilus extension ([69bfa2f](https://github.com/rgon/ncrsDesktop/commit/69bfa2f2fe7614b16fdd244e2aa22c598dba242d))
* add CHANGES IPC command for live emblem refresh after Keep Locally ([1ea8f11](https://github.com/rgon/ncrsDesktop/commit/1ea8f11deec2d9c70197135e39598829bb97c1f0))
* add CLI argument parsing with --offline, --config, and --mount-point flags ([1409a80](https://github.com/rgon/ncrsDesktop/commit/1409a800e2227112ad729f86fc9d86cea0b2b8d6))
* add configurable cache cleanup interval (default 1h) ([f0dfe4e](https://github.com/rgon/ncrsDesktop/commit/f0dfe4eb7b68d6f23561cf1a97e71f477677ca1e))
* add ConflictsView and ErrorsView components ([c3d0a36](https://github.com/rgon/ncrsDesktop/commit/c3d0a3681fc9088798e14af33049e0700c4c9ad4))
* add Credentials auth abstraction for basic and bearer token support ([61d6488](https://github.com/rgon/ncrsDesktop/commit/61d6488015b25078a34ee38d976b2237125f6dcf))
* add deb build script ([8169ea6](https://github.com/rgon/ncrsDesktop/commit/8169ea68646ba68cb717a02f721262113c402e80))
* add exclude_folders config to hide folders from FUSE mount ([78ae3cc](https://github.com/rgon/ncrsDesktop/commit/78ae3ccdd0109dc051465059907aa9d9e3e1ae95))
* add filename validation module matching Nextcloud server rules ([6bcf3d7](https://github.com/rgon/ncrsDesktop/commit/6bcf3d7cbd66a005edd5d2d07fb0710730443980))
* add frontend plugin registry and plugins view ([549cefe](https://github.com/rgon/ncrsDesktop/commit/549cefec842dbfe7e6de870345b41c4cefa7d578))
* add fuse_notify and mutation_journal modules ([07adcf2](https://github.com/rgon/ncrsDesktop/commit/07adcf2237c526cbe664dee679ae352884120315))
* add GNOME Shell search provider for server-side Nextcloud search ([29e4595](https://github.com/rgon/ncrsDesktop/commit/29e4595540f8dfe53224a961f15434b51960fbae))
* add info-level tracing across FUSE, IPC, PROPFIND and Nautilus extension ([1d77751](https://github.com/rgon/ncrsDesktop/commit/1d7775197b86b63c23a1f666e200cb7405b2ecbe))
* add keep_paths config to auto-keep remote paths on startup ([4cdf9a8](https://github.com/rgon/ncrsDesktop/commit/4cdf9a870a3fc55638592cfb61095e7a186447ae))
* add Nautilus columns for sync, sharing, permissions, owner, and size ([357a3fd](https://github.com/rgon/ncrsDesktop/commit/357a3fd3cb391c550986c04d2f22285a7f72b32f))
* add nc_passwords plugin crate with API client ([3d061f3](https://github.com/rgon/ncrsDesktop/commit/3d061f38f0a2b37e232003e5dcbd274fd68963c3))
* add nc:// URI handler for Nextcloud Edit Locally support ([fa604b4](https://github.com/rgon/ncrsDesktop/commit/fa604b48ec945ad6e98b608ce520a8e3fa6dac50))
* add ncrs_plugin shared crate with plugin trait ([8a31352](https://github.com/rgon/ncrsDesktop/commit/8a31352e1bfd1a4840d204c585f043a89a431371))
* add Nextcloud Notify Push WebSocket client for real-time cache invalidation ([5ca11cf](https://github.com/rgon/ncrsDesktop/commit/5ca11cf565f39f8e5c7c48e3952391d69660a633))
* add Nextcloud unified search API to ncrs_core ([b538ea5](https://github.com/rgon/ncrsDesktop/commit/b538ea5b60b3097a896426722d7ee579e9092ad6))
* add oc:permissions and oc:fileid to PROPFIND properties ([d0017d2](https://github.com/rgon/ncrsDesktop/commit/d0017d25353e115bfe246aadefd7f91b4279614f))
* add offline resilience with connectivity monitor and cache-only fallback ([747b9c9](https://github.com/rgon/ncrsDesktop/commit/747b9c98416c1c948bf6c8693032a4c6dfb96547))
* add PasswordsView frontend and register in plugin UI ([07e33de](https://github.com/rgon/ncrsDesktop/commit/07e33deef1f862b1517db0b54caee575ccbe2c17))
* add plugin registry and tray integration ([06a5cf9](https://github.com/rgon/ncrsDesktop/commit/06a5cf96bfdba4ec3f17feb68401d915e80fd4b1))
* add provider type filtering to search ([d77319f](https://github.com/rgon/ncrsDesktop/commit/d77319f68c9f1ab9a64c3c1a99d3ba1bfa02d8e5))
* add resolve_fileids() WebDAV SEARCH by fileid ([a13eaa0](https://github.com/rgon/ncrsDesktop/commit/a13eaa04566741592fd35a62566218b2c6e2e67b))
* add SEARCH IPC command and Nautilus search dialog for Nextcloud ([38e6019](https://github.com/rgon/ncrsDesktop/commit/38e6019875bcdab86e2e0ee0cf1050338951a043))
* add search_nextcloud Tauri command with parallel provider queries ([3aee79a](https://github.com/rgon/ncrsDesktop/commit/3aee79ab112108f695895663a80d3f011d4c492e))
* add SearchView component with debounced NC unified search ([f41eb99](https://github.com/rgon/ncrsDesktop/commit/f41eb993c9551d21e98e241ef65a7f4eab9c9f22))
* add systemd user service and deb packaging metadata ([7b7333b](https://github.com/rgon/ncrsDesktop/commit/7b7333b05ba2d90bc27d1967f5df75e98b677377))
* add WebDAV write operations with FUSE callbacks and ETag conflict resolution ([bdc122a](https://github.com/rgon/ncrsDesktop/commit/bdc122afd63032d03321107a0c2ff11ec42d15ce))
* add window-context detection for auto-suggesting passwords based on active browser tab ([ea1c5ec](https://github.com/rgon/ncrsDesktop/commit/ea1c5ec0e13f3aba2d5622229d7121ad7d74aa4b))
* **auth:** Nextcloud Login Flow v2 — trigger when password missing ([f8293e7](https://github.com/rgon/ncrsDesktop/commit/f8293e76f4337915c3183c547e1d5ffe72b819f3))
* **auth:** system keyring for app password; warn on insecure config perms ([35e074b](https://github.com/rgon/ncrsDesktop/commit/35e074b5377e9ed53a971ba91fc542fb9795c35d))
* auto-create mountpoint if doesn't exist ([9de011b](https://github.com/rgon/ncrsDesktop/commit/9de011bff871f1a25ca7191c8493a71e0cdc9666))
* background file caching on open, keep-locally IPC+menu, fix emblems, add debug logging ([f792818](https://github.com/rgon/ncrsDesktop/commit/f79281806ebbe772fb60f5940303276bacc7413e))
* cache directory ls and update with notify-push ([415a87f](https://github.com/rgon/ncrsDesktop/commit/415a87fd0ef0208c544e90eb9bbc13a12cba7e04))
* cache streaming reads to disk when full file is covered, with configurable read-ahead ([54b441d](https://github.com/rgon/ncrsDesktop/commit/54b441d9f1c707ab44d89d5c8ddce2d06dc19e5c))
* **calendar:** add nc_calendar plugin with GOA/CalDAV auto-registration ([e58b41e](https://github.com/rgon/ncrsDesktop/commit/e58b41e476d4c232a6a2db4264ceca240ebe8eaa))
* **ci:** build the deb via build-deb.sh and verify it with test-deb.sh ([6eadd86](https://github.com/rgon/ncrsDesktop/commit/6eadd86600697fbc195d7cb042509c786ca5683a))
* **cli:** add --url/--username/--password overrides and remove http3 probe ([a24a1f0](https://github.com/rgon/ncrsDesktop/commit/a24a1f0e8c02a669cc322ba9cbfb4cb8f99e4e17))
* **config:** auto-normalize DAV URL from any user-supplied format ([1f582b6](https://github.com/rgon/ncrsDesktop/commit/1f582b6ba38341d6b439ac5f7a50852754409db0))
* **config:** enable http3 by default; add explanatory comments to advanced settings ([4b34dbc](https://github.com/rgon/ncrsDesktop/commit/4b34dbc3a33bfefb601c0313ea63d4501648848a))
* **core:** add --print-default-config flag and explicit ncrs bin target ([4b9dd9d](https://github.com/rgon/ncrsDesktop/commit/4b9dd9ddad8906915c21ebf4f5eb90c5694803aa))
* **core:** add STATE, PAUSE and RESUME IPC verbs for external clients ([3b0bb03](https://github.com/rgon/ncrsDesktop/commit/3b0bb034f761c1bf4e6981ec13e9872bf4065ae3))
* deferred subdirectory readdir, partial-download icons, evict command, cache recovery, and fix web URLs ([a4c2a2b](https://github.com/rgon/ncrsDesktop/commit/a4c2a2b91dfcc91a6a866ee3fe3f10da1046db5e))
* derive FUSE mode bits from Nextcloud oc:permissions flags ([32f7be3](https://github.com/rgon/ncrsDesktop/commit/32f7be37b623cd87bed5ccd34eccaca9652012a7))
* derive Serialize/Deserialize for DavEntry for cache persistence ([7f8bc65](https://github.com/rgon/ncrsDesktop/commit/7f8bc65c9ea3f1e12ea5f06c5a0dcfa55f64bb98))
* detect shared files via oc:share-types and show shared emblem in Nautilus ([22d65df](https://github.com/rgon/ncrsDesktop/commit/22d65df9bbbcb7eae2e80e4e8cee1e29538c0f64))
* dir cache persistence, sync reads, HTTP/3 client, child prefetch batching ([1ced20d](https://github.com/rgon/ncrsDesktop/commit/1ced20d4187746245546bb379d58b47a00466aaa))
* expose error_log, transfer_map, and journal through Tauri state and commands ([ab5a4e4](https://github.com/rgon/ncrsDesktop/commit/ab5a4e459c60275c033c5ee4b99fa1c2f5cd266c))
* extend SyncProgressView with transfers and error indicators ([609512d](https://github.com/rgon/ncrsDesktop/commit/609512dac488fa72c96e864e6eaaab76c8d359c6))
* **fuse:** auto-delete stale GIO atomic-write temps from server on readdir ([e02bbfd](https://github.com/rgon/ncrsDesktop/commit/e02bbfd80fd5a2a05e5c75ccef51604a002b6e3e))
* **fuse:** implement readdirplus to bundle entry attributes and cut per-file getattr ([1ef19a9](https://github.com/rgon/ncrsDesktop/commit/1ef19a9b166637854471cf3140f890fc799fd464))
* **fuse:** serve MIME type via user.xdg.mime.type xattr to skip GIO content sniffing ([53a1f18](https://github.com/rgon/ncrsDesktop/commit/53a1f18407e7c839cc56318b5a6fad119b263a5e))
* ghost entries + inotify events for all FUSE ops with Nautilus DBus reload ([8003ed4](https://github.com/rgon/ncrsDesktop/commit/8003ed4e8780dfd6ca77de0003e7f33c4e5e0c15))
* **gui:** add description hints to advanced settings toggles ([a129dcb](https://github.com/rgon/ncrsDesktop/commit/a129dcbc4b447653e11b97f4919e37dd0d860ffc))
* **gui:** add purge local cache action that re-downloads fresh copies while preserving unsynced edits ([c7225d3](https://github.com/rgon/ncrsDesktop/commit/c7225d3736e70b30445dd85b717a92bb59fe38c0))
* **gui:** add Remount button to settings footer ([3838009](https://github.com/rgon/ncrsDesktop/commit/38380091e36837c784b5a39bdf6ac86cc73b9741))
* **gui:** add settings view with yaml config editing and version indicator ([63b3154](https://github.com/rgon/ncrsDesktop/commit/63b315440767e394c001b970a35a8c121296188c))
* **gui:** attach to a running daemon over IPC instead of mounting a second time ([0f9c804](https://github.com/rgon/ncrsDesktop/commit/0f9c804a64bfa21507af1ef52ab641d54afddc09))
* **gui:** left-click tray icon opens window, right-click shows menu ([09d9295](https://github.com/rgon/ncrsDesktop/commit/09d92952867910e1d16d69c87ebc33a6887fbf64))
* **gui:** mirror journal and conflicts from daemon in attach mode ([6e43466](https://github.com/rgon/ncrsDesktop/commit/6e43466534b63a690d65732d401b0638369af321))
* handle FUSE unmount event with shutdown flag, GUI remount ([e6238f8](https://github.com/rgon/ncrsDesktop/commit/e6238f8764f1191a9e191837b853c149971ca4ac))
* **hpb:** add degraded state and orange tray icon for notify_push failures ([0cfa568](https://github.com/rgon/ncrsDesktop/commit/0cfa568196cc4aedf7c725cdb19974e4b0c5896c))
* HTTP Range partial reads with 2MB read-ahead buffer for streaming ([9a10ebc](https://github.com/rgon/ncrsDesktop/commit/9a10ebcbfd424280a4b5f86bd65fc90f760f3bb1))
* **http:** probe HTTP/3 at startup, fall back to HTTP/2 if QUIC unavailable ([8adc9b2](https://github.com/rgon/ncrsDesktop/commit/8adc9b22765a3b51f5449f33458c333b57ac5b1e))
* implement chunked uploads for files larger than 10MB ([a215449](https://github.com/rgon/ncrsDesktop/commit/a2154494fa9295e796d54d318efda65103d31de3))
* implement pause sync via shared paused flag in core and GUI ([a8f3134](https://github.com/rgon/ncrsDesktop/commit/a8f31347810fe9413efc0e6abfa84118f408fa36))
* incremental readdir with channel-based streaming for cold cache ([01c3736](https://github.com/rgon/ncrsDesktop/commit/01c37366d03483fc841e6bc86e55a409adc1f32a))
* **ipc:** add DETAILDIR batch command for directory metadata ([f15d22d](https://github.com/rgon/ncrsDesktop/commit/f15d22d46fb7720265b40237d3c56ea5f5c8dce8))
* **ipc:** add DETAILDIR to batch a directory's child metadata into one reply ([e1eb509](https://github.com/rgon/ncrsDesktop/commit/e1eb509cc744703b1471c691bdb4a5a99462d90c))
* **ipc:** add VERSION handshake so daemon/extension protocol mismatch is logged ([8e13f37](https://github.com/rgon/ncrsDesktop/commit/8e13f37b93bd19725eb183b8bab709d9da1050cf))
* **ipc:** push daemon state to subscribers via a SUBSCRIBE verb so the GUI stops polling ([28efecd](https://github.com/rgon/ncrsDesktop/commit/28efecdc911ecb58ab5c72f06d01364a196a355f))
* **issues:** add Clear all button for conflicts in the warnings tab ([4a49942](https://github.com/rgon/ncrsDesktop/commit/4a499429f66362f0da9ec77f3cfca26b44166f3e))
* **issues:** add per-item error dismiss button ([fe5fde9](https://github.com/rgon/ncrsDesktop/commit/fe5fde94a559e80593219c983610c65e6795c343))
* **logging:** replace env_logger with tauri-plugin-log to surface warnings in DevTools ([fd341e9](https://github.com/rgon/ncrsDesktop/commit/fd341e90918222f406362113fd7422f57bfe2198))
* **logging:** wire @tauri-apps/plugin-log JS package and attachConsole for DevTools output ([9eff244](https://github.com/rgon/ncrsDesktop/commit/9eff2442d0bbc62aeabcaec7f70908b5e1df9328))
* **login:** pre-fill server URL from existing config ([bd7d003](https://github.com/rgon/ncrsDesktop/commit/bd7d003ed4c7ac9a7ad14870dcb79561f8751c49))
* open file results in file browser with view-online button ([d844d80](https://github.com/rgon/ncrsDesktop/commit/d844d8013f340d4d47f2087169330d0673b7af97))
* **packaging:** add AppStream metainfo with OARS rating and hardware hints ([880918b](https://github.com/rgon/ncrsDesktop/commit/880918bb77e1e5c5ba7975b106275525f6c4c2aa))
* **packaging:** ship GUI desktop entry, icons, autostart and example config in the deb ([e399413](https://github.com/rgon/ncrsDesktop/commit/e39941328fdf3657952b469d35c9166712a6bfb5))
* populate IPC detail for directories themselves via PROPFIND self-entry ([b7b0bb8](https://github.com/rgon/ncrsDesktop/commit/b7b0bb80ee39ba4fc379cf5fd3e850a15cb9ad17))
* prefetch NC preview thumbnails into XDG cache on directory listing ([16ed616](https://github.com/rgon/ncrsDesktop/commit/16ed6168a2532018df6d89bbc681aac276b09514))
* **preview:** fetch thumbnails for RAW camera formats regardless of has_preview flag ([98a92cf](https://github.com/rgon/ncrsDesktop/commit/98a92cf03e6bcd3b0c5853d8a92d2bf189bf5e1c))
* **preview:** touch FUSE atime after thumbnail write to auto-refresh Nautilus ([5ccd830](https://github.com/rgon/ncrsDesktop/commit/5ccd830948ed9730dcdb0ef172acb0f68ebeff3c))
* proactively propfind invalidated etags of /* at boot ([18a9e2e](https://github.com/rgon/ncrsDesktop/commit/18a9e2eedf9e1d1a0cc62ef85bb64f257af22a49))
* raw PROPFIND with NC properties (has-preview, etag, oc:size), replace remotefs for listings ([c78fd46](https://github.com/rgon/ncrsDesktop/commit/c78fd468d0ef57aa3a0cde2423cd2a204a9da4da))
* redesign PasswordsView as quick-access popup with click-to-copy ([6a28378](https://github.com/rgon/ncrsDesktop/commit/6a283789f1e2146f429a9dafcfd67afbcf92a8d8))
* register nc_passwords plugin in tauri backend ([853a457](https://github.com/rgon/ncrsDesktop/commit/853a45791512e0b1ca16975eaaeb7a824e39063b))
* replace stub 'Add account' with working Log out button ([c16bf42](https://github.com/rgon/ncrsDesktop/commit/c16bf42bab588e16e099f484e389e65d686a9548))
* separate kept and cached files into distinct directories with per-file status ([9157a48](https://github.com/rgon/ncrsDesktop/commit/9157a485777ecc44fb60891608f39da04569db10))
* set x-gvfs-notrash mount option to prevent Nautilus trash dirs on Nextcloud ([6b10f5c](https://github.com/rgon/ncrsDesktop/commit/6b10f5cb83f5b231eca1e550289a43f61c86ed2f))
* **settings:** add GNOME GIO intermediate auto-cleanup toggle ([7fa2692](https://github.com/rgon/ncrsDesktop/commit/7fa269218da1f2c84aa6692e0cd21d165dd2e645))
* show correct size and uploading emblem for newly written files ([b65f7f0](https://github.com/rgon/ncrsDesktop/commit/b65f7f0994b49d8f7922b189174b82aae027657a))
* show storage usage in GUI for kept files, cache, and server quota ([8c8b207](https://github.com/rgon/ncrsDesktop/commit/8c8b207ede8bd8c5e13c48a9e266b73ec9074c0f))
* stale-while-revalidate for directory cache, serve cached listings immediately ([96d6c20](https://github.com/rgon/ncrsDesktop/commit/96d6c2029b19fd5985b7aa85c7e7119df616e9ca))
* support Nextcloud remote wipe to delete local data on server command ([0dd7c38](https://github.com/rgon/ncrsDesktop/commit/0dd7c386937e5c6dfa28b904aeb92a0c07a9e292))
* **sync:** keep local edits on upload failure, mark pending-sync in UI, and retry queued mutations while online ([8dcfa54](https://github.com/rgon/ncrsDesktop/commit/8dcfa549920d075ebca20071ba9a4cbca41f1637))
* **tauri:** add dismiss_error command to remove single error by timestamp ([4e71616](https://github.com/rgon/ncrsDesktop/commit/4e7161609e16ec46dda2ca4cbcfe27b0340ab9a9))
* **theme:** fetch Nextcloud server accent color from capabilities and apply to UI ([0371c1b](https://github.com/rgon/ncrsDesktop/commit/0371c1b56610140522073c8db12250be99520afc))
* thread http3 flag through notification and search clients ([f9ad7fe](https://github.com/rgon/ncrsDesktop/commit/f9ad7fe2ec9cbe8a3038f5fd544b43e68daaed62))
* **thumbnailer:** add CR3/CR2 raw thumbnail support via embedded JPEG preview ([0c39c1a](https://github.com/rgon/ncrsDesktop/commit/0c39c1ad47f2c45fa5127d2894a2184fd8129c5c))
* **thumbnailer:** add ncrs-thumbnailer for PDF with evince fallback for local files ([180dc35](https://github.com/rgon/ncrsDesktop/commit/180dc35a14f716b5ab6c47e4836a09da5a897c6a))
* **thumbnailer:** fetch NC preview via IPC instead of reading local raw file, expand to all registered RAW MIME types ([b5d44c4](https://github.com/rgon/ncrsDesktop/commit/b5d44c48cedc9394d9c4cfe22ce56a1975406de2))
* **thumbnailer:** render images via Nextcloud preview API, mount-scoped to avoid full downloads ([c85f549](https://github.com/rgon/ncrsDesktop/commit/c85f549e2e4c8bbbc892b7467c8d6e7af2f92988))
* **ui:** detect system dark/light mode and add design tokens ([0f244f5](https://github.com/rgon/ncrsDesktop/commit/0f244f569eb577130f6abd3bea6d13d66c321fc7))
* update nautilus extenision on runui ([b752dfa](https://github.com/rgon/ncrsDesktop/commit/b752dfad782d8ab477a18aadadebc7ed4f4b1545))
* upgrade reqwest 0.11 to 0.12 with HTTP/3 QUIC support ([ec801f1](https://github.com/rgon/ncrsDesktop/commit/ec801f12fee69d808ce6cb425b6b42336b6328d6))
* wire errors, transfers, and conflicts state into main page ([6e67051](https://github.com/rgon/ncrsDesktop/commit/6e67051d7bf94e2272be15a729b2e1d2919031ca))


### Bug Fixes

* absolute window positioning wayland bypass had wrong offset ([44de636](https://github.com/rgon/ncrsDesktop/commit/44de6369a40f7a9599f7adb4df2a36a5456ca9e7))
* add done flag to stream buffer so waiters fail fast on short downloads ([e5205a0](https://github.com/rgon/ncrsDesktop/commit/e5205a029fd1153ba506e205f63e0f6abaf9508a))
* **auth:** surface 401 as error state and fix attached_poll_loop swallowing daemon errors ([5d44377](https://github.com/rgon/ncrsDesktop/commit/5d44377564fbdf54f64382549032d25c5d1c15c3))
* batch-populate IPC maps before readdir reply for immediate column data ([fc322f2](https://github.com/rgon/ncrsDesktop/commit/fc322f2a17405779ffda4973a8816d03363450d0))
* bound IPC connections to 64 with 60s read timeout to prevent thread leaks ([5917299](https://github.com/rgon/ncrsDesktop/commit/5917299fdb22c20cdebe5240b2904ba323006b04))
* **build:** sync Cargo.lock to workspace version 0.1.12 ([6ac52f7](https://github.com/rgon/ncrsDesktop/commit/6ac52f7590988baf7f4da5b18daeb1a5a0c098eb))
* **cache:** delete orphaned write_* staging files after upload completes ([af1686b](https://github.com/rgon/ncrsDesktop/commit/af1686b3b4dcc2786c8bfeff1c59d51870b6e971))
* **calendar:** manage own GOA account; never reuse manually-added entries ([7289db0](https://github.com/rgon/ncrsDesktop/commit/7289db015bf6b35c341c3f0f08eded613fc73acd))
* cancel previous download when new range read starts for same fh ([7177ee4](https://github.com/rgon/ncrsDesktop/commit/7177ee4a30af799771151e6af813b7cd390a1a7f))
* check dir cache before PROPFIND in keep-locally to avoid querying file paths as directories ([87a4e68](https://github.com/rgon/ncrsDesktop/commit/87a4e689a368096ac494ac4e53d11112cf242983))
* **ci:** add actions:read permission for artifact downloads ([45cdb14](https://github.com/rgon/ncrsDesktop/commit/45cdb14e1976f0f019cdfba2eb6d804523ad0432))
* **ci:** bump GHA actions to latest versions ([02998c9](https://github.com/rgon/ncrsDesktop/commit/02998c9e66d11b12f38e4b8b611bb93074b516a6))
* **ci:** cache release binaries; skip e2e for release-please PRs ([c8b46d6](https://github.com/rgon/ncrsDesktop/commit/c8b46d6a78e3118ab99d51b0a6b5ea37c5923594))
* **cicd:** pin pnpm version ([896c25b](https://github.com/rgon/ncrsDesktop/commit/896c25b18d07a5f9feb4a40ca2bd0ae2a704f6ad))
* **cicd:** proper release-please config ([9798a78](https://github.com/rgon/ncrsDesktop/commit/9798a78c30d8ed21183b734e12a809bfb193599a))
* **ci:** check out github.sha in build-release to guarantee binary cache hit ([8aa42a7](https://github.com/rgon/ncrsDesktop/commit/8aa42a7891f78c7c9517ebc2a17de68ef9b3d405))
* **ci:** create draft releases and publish only after successful .deb upload ([a20d247](https://github.com/rgon/ncrsDesktop/commit/a20d247ab6ef7824f6bdaf757c651b1f2da5fdc2))
* **ci:** downgrade download-artifact to v7 to match upload-artifact v7 ([84cddc8](https://github.com/rgon/ncrsDesktop/commit/84cddc8fd0a2d6c7712abb79ff2aad610c9a7642))
* **ci:** drop unused node/pnpm steps from test job ([e592f89](https://github.com/rgon/ncrsDesktop/commit/e592f8944e334a96f2c244e8d39c7260d45008a9))
* **ci:** fix sccache GHA backend and add Cargo registry cache ([6e8385a](https://github.com/rgon/ncrsDesktop/commit/6e8385a8ff792da6b3570bb5fcc2c61dcacb8a00))
* **ci:** gate release-please on e2e and run e2e on release PRs so a red suite blocks releases ([690abac](https://github.com/rgon/ncrsDesktop/commit/690abac38d4bdfd04d6dba6bcb7e606a7a4ffda1))
* **ci:** install pnpm via npm instead of pnpm/action-setup ([c270559](https://github.com/rgon/ncrsDesktop/commit/c270559a4cff74dbec5b1d7f90dbd81a5a28a737))
* **ci:** move update-lockfile guard to step level to prevent skip propagation ([d9316d2](https://github.com/rgon/ncrsDesktop/commit/d9316d22974378f29fda5966eb580a49c93f85f1))
* **ci:** pass token explicitly to release-please action ([3b2ce31](https://github.com/rgon/ncrsDesktop/commit/3b2ce3199eecf0b1627815d8672befb075ee4fd2))
* **ci:** remove global sccache rustc-wrapper; bump sccache-action to v0.0.10 ([9100bf0](https://github.com/rgon/ncrsDesktop/commit/9100bf05824d69f20801d11915e50ceef794d3e2))
* **ci:** replace upload/download-artifact with actions/cache for cross-job binary sharing ([8f6a812](https://github.com/rgon/ncrsDesktop/commit/8f6a81257c73794068811e3906bc3470d8b9c385))
* **ci:** single build job; reuse binary in e2e and release ([2d7e7bb](https://github.com/rgon/ncrsDesktop/commit/2d7e7bb722d9f08efdfbb43f031f638bd3ddaf08))
* **ci:** update debian version for CI test to run correctly, fix build and cache usage ([c0d099d](https://github.com/rgon/ncrsDesktop/commit/c0d099de1cff713dcc8d5885e37e12ac3497b805))
* clean up stale zero-byte write_* temp files on daemon startup ([cdb1599](https://github.com/rgon/ncrsDesktop/commit/cdb1599b1c2c8ad04073b8ed15731531ef73d49b))
* **core:** adopt orphaned mount-point writes instead of refusing to remount ([fdc45cd](https://github.com/rgon/ncrsDesktop/commit/fdc45cd186afa0a8fd8c2ac215528f196a5c7dfc))
* **core:** broaden is_transient_network_err to prevent FUSE EIO on transient network failures ([6f9ff75](https://github.com/rgon/ncrsDesktop/commit/6f9ff75a5cd15b69b95ef5435399852059d93cac))
* **core:** clear deleting guard on journal replay ServerError and MAX_ATTEMPTS paths ([f5e02a4](https://github.com/rgon/ncrsDesktop/commit/f5e02a4c1838ad4ae3eba041c8f19fc32912f623))
* **core:** parallelize boot validation and update detail maps on background dir refresh ([553b5fe](https://github.com/rgon/ncrsDesktop/commit/553b5fea7f0481eed6128048fa0a6097c4f16e88))
* **core:** prevent racing PROPFIND from re-surfacing in-flight deleted files and dirs ([a441d36](https://github.com/rgon/ncrsDesktop/commit/a441d361ec96db11b45cef0e1d8f00b931b59972))
* **core:** refuse to mount over a live mount or non-empty dir, remove mount dir on exit ([d9a6412](https://github.com/rgon/ncrsDesktop/commit/d9a6412113f1f5194b4f96d9e1a8259161d9a720))
* **core:** remove erroneous MKCOL 409 idempotent arm that silently dropped journal entries ([16d81e7](https://github.com/rgon/ncrsDesktop/commit/16d81e7438ba65c92462755becf5dae0ef5ddf02))
* **core:** surface PROPFIND auth/network errors to readdir and GUI error log ([1c06516](https://github.com/rgon/ncrsDesktop/commit/1c065164e60df6aafa3b8b857958114814255b7b))
* **core:** validate mount point before touching the IPC socket and shared state ([f196be8](https://github.com/rgon/ncrsDesktop/commit/f196be82d76580cd9f0b520f0a33d9487af817a1))
* correct Nextcloud oc:permissions flag mapping in Nautilus extension ([6685d6f](https://github.com/rgon/ncrsDesktop/commit/6685d6fa1f2745add45b7324a068a0de350333ac))
* dirty file path after PUT so Nautilus clears uploading emblem automatically ([2f39435](https://github.com/rgon/ncrsDesktop/commit/2f39435dae58178ef3a65549bca495bcba0af5e6))
* don't invalidate directories on boot if etag not changed, better atime notify-push ignore after our own propfind to prevent infinite loops ([7e5074a](https://github.com/rgon/ncrsDesktop/commit/7e5074a725ef5a1a5237d6a2885bd8dbd5444373))
* drop AutoUnmount — fuser 0.17 requires allow_other with auto_unmount ([8d2da94](https://github.com/rgon/ncrsDesktop/commit/8d2da94898881c26d4fe7cbdc0955e481ce2e511))
* **e2e:** install procps in e2e container for pgrep in scenario 19 ([c41482b](https://github.com/rgon/ncrsDesktop/commit/c41482b8d7edbcc5ecfa98b069f44216566d23dd))
* **e2e:** mkdir adopt dir on real fs after lazy unmount so orphaned write lands ([788dd0f](https://github.com/rgon/ncrsDesktop/commit/788dd0f1b661d80d31db74f7899fc6d09b9bb979))
* enable rustls-tls for tungstenite and retry notify_push discovery on failure ([871f45e](https://github.com/rgon/ncrsDesktop/commit/871f45e898be7295540f9dab1720c76c2c9cc830))
* enforce Nextcloud oc:permissions via DefaultPermissions FUSE mount option; add perms_to_mode tests ([ced8a5c](https://github.com/rgon/ncrsDesktop/commit/ced8a5c67b9bc4b5b3a9b349847b230a7e894288))
* fetch parent directory on lookup cache miss after daemon restart ([a79b075](https://github.com/rgon/ncrsDesktop/commit/a79b0755dc50f799f6f3472fde9c5ca71f43b325))
* **fuse:** delete all GIO temps on PROPFIND, not just age-threshold ones ([921d2a7](https://github.com/rgon/ncrsDesktop/commit/921d2a77d076aede28f65c00b91fdfaf21ab86a1))
* **fuse:** evict all five maps on rmdir to match unlink cleanup ([6e8d4cb](https://github.com/rgon/ncrsDesktop/commit/6e8d4cba920a3207ef0ac3206341c698ce7b1c40))
* **fuse:** evict stale file_cache copy when a file changes on the server so reopens aren't served old bytes ([7804763](https://github.com/rgon/ncrsDesktop/commit/780476323bb7ec23d866604fb788f673140ff9a3))
* **fuse:** fail fast on a blackholed read by disabling read-client idle pooling and treating reqwest send errors as network-down ([0b12058](https://github.com/rgon/ncrsDesktop/commit/0b1205876f5cc3bfb1705dabeee01c7d01c0414e))
* **fuse:** fallback to octet-stream for MIME opens with no server content-type ([08e5673](https://github.com/rgon/ncrsDesktop/commit/08e5673e7ddf7c49f714be670db0b5cbbf190150))
* **fuse:** flip offline eagerly on network-down and add HTTP connect_timeout so an offline save falls back to cache instead of hanging ([8992a77](https://github.com/rgon/ncrsDesktop/commit/8992a773814c439dac62d1f23f96712288a3ce5b))
* **fuse:** gate read fast-paths on cache-vs-remote freshness so a server-edited file isn't served stale at the new size ([bcc3cc8](https://github.com/rgon/ncrsDesktop/commit/bcc3cc862d9f9bf8f9da22f0a9db328dfa1e63ff))
* **fuse:** guard MIME magic intercept to sz&lt;=16384 to avoid corrupting file copies ([7ec0e7a](https://github.com/rgon/ncrsDesktop/commit/7ec0e7a7e06d9fe5477c5a07cf9199f8c1ad552c))
* **fuse:** guard newly created files in uploading set to prevent ENOENT race ([9462955](https://github.com/rgon/ncrsDesktop/commit/94629552fc2c6551995b9edfca4a413cd595ed9b))
* **fuse:** make MIME-detect fallback category-aware so unknown binaries stop showing as text ([2fb74b5](https://github.com/rgon/ncrsDesktop/commit/2fb74b5de37593a986cb0a82836759c8fa9e5e4e))
* **fuse:** map image/x-dcraw to TIFF magic so camera RAW files show as images not text ([f57a0ce](https://github.com/rgon/ncrsDesktop/commit/f57a0ce98baf3259a2d4df127272eb94a6eecb0f))
* **fuse:** open MIME-detect handle O_DIRECT to stop page-cache poisoning truncating reads ([3acaf55](https://github.com/rgon/ncrsDesktop/commit/3acaf5546703d7d8fc8d03c29d945528d6e3d891))
* **fuse:** preserve in-flight uploads during concurrent dir cache PROPFIND refresh ([c41f68e](https://github.com/rgon/ncrsDesktop/commit/c41f68eae36ca25a0588f2585ce6893f084c650e))
* **fuse:** prioritize pending-PUT staging over cached copies so reads return the newest local write ([953ce66](https://github.com/rgon/ncrsDesktop/commit/953ce66f4892053d7b7d93e93d33347ae86c3a34))
* **fuse:** raise MIME magic intercept guard to 32K to cover kernel read-ahead ([8e104c9](https://github.com/rgon/ncrsDesktop/commit/8e104c9eec916661d7f97c8420ba7a7602688639))
* **fuse:** reconcile stale getattr size on read so a server-edited file isn't served truncated; pin cache freshness per open handle ([f2f76e1](https://github.com/rgon/ncrsDesktop/commit/f2f76e15469779f50e8efeab1eaf98ca9307436f))
* **fuse:** revalidate dir etag in background on every readdir so changes missed by notify-push surface without a cache purge ([757d97f](https://github.com/rgon/ncrsDesktop/commit/757d97f1fd507d8497460d69c3931a61a86367c1))
* **fuse:** serve reads of not-yet-uploaded files from pending PUT staging to avoid EIO on save-then-reopen ([f9edd74](https://github.com/rgon/ncrsDesktop/commit/f9edd74851e845580610cb57c53efc9c31c0cdcc))
* **fuse:** skip the ensure_file_cached fallback on read when offline so a blackholed server fails fast instead of retrying connect timeouts ([cce9add](https://github.com/rgon/ncrsDesktop/commit/cce9addf87eb2ae6e707f0d0ea73f7cd0d6f0f7e))
* **fuse:** update inode map on rename so saved files don't vanish ([233b5db](https://github.com/rgon/ncrsDesktop/commit/233b5dbf63c62a4faacee613d01247762ef90113))
* **fuse:** use staging file size in rename optimistic update; add flush/move logging ([5c90ded](https://github.com/rgon/ncrsDesktop/commit/5c90dedf9d227d8d7ddc658e81cae7c61ca7ce98))
* **fuse:** use TTL=0 for readdirplus entry attrs to prevent stale-size data loss ([2585b34](https://github.com/rgon/ncrsDesktop/commit/2585b34aa0bf7df4e5b426dbf8f97ccd6a0e8806))
* **fuse:** wait for in-flight PUT before issuing MOVE on rename ([dd7432f](https://github.com/rgon/ncrsDesktop/commit/dd7432f34121acdb0e1114f81194efe30708e079))
* gate child-dir PROPFIND prefetch behind aggressive_prefetch and add HTTP request throttle ([5ec0d5b](https://github.com/rgon/ncrsDesktop/commit/5ec0d5b29e682d5a0bb592f4ff2aabd17bd4b2ea))
* **goa:** write keyring credentials before accounts.conf to eliminate auth race ([70e5aa5](https://github.com/rgon/ncrsDesktop/commit/70e5aa578936b4e42f8893a799da8e0e7774cc1b))
* green-checkmark after upload; NC properties appear via forced PROPFIND ([f42c403](https://github.com/rgon/ncrsDesktop/commit/f42c4030e517ddc2541be70165bb8a72ec560063))
* guard unlink/rmdir/rename with NC D/N/V flags; block delete in create-only shared dirs ([476a2bd](https://github.com/rgon/ncrsDesktop/commit/476a2bd7bcf728e72b772dac56e8100f1a8a1f69))
* **gui:** embed tray icons at compile time and enforce a single app instance ([1afc89d](https://github.com/rgon/ncrsDesktop/commit/1afc89d6cbc519ec435f558bd1ff9b4e29c72ab8))
* **gui:** fall back to first available monitor and re-fit on scale change so the overlay covers the screen on Wayland scaled displays ([d018168](https://github.com/rgon/ncrsDesktop/commit/d018168d54cf3918bc9caefce20eb46f50cd794f))
* **gui:** pin plugin bare imports to local node_modules for production builds ([e49e969](https://github.com/rgon/ncrsDesktop/commit/e49e969dd98a28927cd41757463838be5d1965e1))
* **gui:** prevent floating panel from shrinking on HiDPI-scaled displays ([80161ed](https://github.com/rgon/ncrsDesktop/commit/80161ed571ae6cb57b4d9ad4f13768d00d44ba49))
* **gui:** regenerate app icons from the ncrs brand mark instead of the tauri template ([c87d7ef](https://github.com/rgon/ncrsDesktop/commit/c87d7efa2aefaead42fb059314a085d2613d4089))
* **gui:** reload user info and theme when window opens before daemon is ready ([759a334](https://github.com/rgon/ncrsDesktop/commit/759a3340a319dd0c1f57e10c0c9d7fb0da799a5e))
* **gui:** size overlay in logical units so fractional-scaled displays don't crop the right-anchored card ([31121af](https://github.com/rgon/ncrsDesktop/commit/31121af28fd2b097e10d099a03fa6a90f800963d))
* **gui:** stop lazy-detaching busy mounts to avoid orphaned writes ([7c283d1](https://github.com/rgon/ncrsDesktop/commit/7c283d1d337910baf311fac26cb2edc312d27238))
* **gui:** use composedPath for click-outside detection of detached nodes ([74922f5](https://github.com/rgon/ncrsDesktop/commit/74922f5d2ada957fbcc80bdff1fc04fc86d19aae))
* harden bearer auth — redact secrets, stream downloads, pre_auth WS, validate creds ([eccecbe](https://github.com/rgon/ncrsDesktop/commit/eccecbe238bc19f00006d2b41eda98c1892ad768))
* harden FUSE, IPC, and Nautilus extension against panics and errors ([40aee6e](https://github.com/rgon/ncrsDesktop/commit/40aee6e3bb78320b7830da0a71872b4587389d1a))
* **http3-probe:** require HTTP/3 response version; reqwest 0.13 silently falls back to HTTP/1.1 ([a276b23](https://github.com/rgon/ncrsDesktop/commit/a276b23b2da99518e34e8af65c64208c14c73a0d))
* **http3-probe:** use async reqwest client with dedicated runtime for reliable H3 detection ([1bc9476](https://github.com/rgon/ncrsDesktop/commit/1bc9476609fff125fddfee4fe089ac3643a7a217))
* **http3:** fall back to HTTP/2 when HTTP/3 connection fails in notifications ([3b217c5](https://github.com/rgon/ncrsDesktop/commit/3b217c538d8d510ef0641db9d9cc2aa590970897))
* **http:** correct misleading http3 alt-svc comment and hint ([dfd851c](https://github.com/rgon/ncrsDesktop/commit/dfd851c8edf2558d22f954859c33d688497ed2b5))
* **http:** remove http3_prior_knowledge; use alt-svc negotiation instead ([239809c](https://github.com/rgon/ncrsDesktop/commit/239809ccbe2a97b1fd92fb6a48237ad2a62c441b))
* **ipc:** code-review fixes — children_map consistency, TOCTOU, stale-entry eviction, fallback scans ([2ae1bb8](https://github.com/rgon/ncrsDesktop/commit/2ae1bb897403eb3e3f41c11aecbe6c2caf2b260e))
* **ipc:** evict directory's own status entry on readdir refresh ([401c743](https://github.com/rgon/ncrsDesktop/commit/401c74358bebea8fd5787d0e71c0c6e74df19826))
* **ipc:** preserve concurrent lookup insertions in children_map readdir rebuild ([5c19fc5](https://github.com/rgon/ncrsDesktop/commit/5c19fc525bc8602c0ee0aa2df7414ddd37ee80b4))
* **ipc:** update detail_map and children_map atomically in readdir rebuild ([e58365e](https://github.com/rgon/ncrsDesktop/commit/e58365e3f032c7135aa3c8b10d06c38f7057a767))
* **issues:** override DaisyUI grid on alert cards, add dismiss × button ([b1a260f](https://github.com/rgon/ncrsDesktop/commit/b1a260f295e78db17a96f8e49e2fb4e275fcc9c4))
* **issues:** replace DaisyUI btn with plain icon-button classes to fix overflow ([9671b18](https://github.com/rgon/ncrsDesktop/commit/9671b182b1f3951d0544d4c891e64d2b216f0aa7))
* **keyring:** delete before save and refresh creds on 401 in notification poll ([d9ff15b](https://github.com/rgon/ncrsDesktop/commit/d9ff15bc19cc2c0849079da5233bc08cd24d219a))
* lazy-unmount stale FUSE mount before remounting on restart ([bd4cec5](https://github.com/rgon/ncrsDesktop/commit/bd4cec563fc25f0e1ffba4beb431f39039084fa4))
* **login:** correct init endpoint to /index.php/login/v2 and strip WebDAV paths from user input ([4410f91](https://github.com/rgon/ncrsDesktop/commit/4410f91c3931b208fcdf161903dd1f6fbfb4c0e2))
* **login:** send User-Agent header so Nextcloud shows app name in OAuth grant page ([e7e9320](https://github.com/rgon/ncrsDesktop/commit/e7e9320e5a59bd708e31f3eba39f835bd11b216f))
* **logout:** preserve server URL in login form after re-login flow ([e58ccaa](https://github.com/rgon/ncrsDesktop/commit/e58ccaa58ba5b789353cac037b02feb1ba372719))
* make search async with cancellation and debounce throttling ([5ac2743](https://github.com/rgon/ncrsDesktop/commit/5ac2743cf20ba0cb3e098edf43db34175366bb80))
* **mount:** remove IPC socket on FUSE teardown so remount doesn't enter attach mode ([bf5c7d5](https://github.com/rgon/ncrsDesktop/commit/bf5c7d553e5ee07fe4a10c2c4f595b37828876c7))
* **mount:** surface FUSE errors to UI and allow remount from error state ([9aa6217](https://github.com/rgon/ncrsDesktop/commit/9aa6217e0842bb4a3355effe93c56bffc9efb236))
* move DETAIL logic into update_file_info_full which Nautilus 4 actually calls ([c61c494](https://github.com/rgon/ncrsDesktop/commit/c61c49420f28251038360dccd5988657c0ad4c88))
* **nautilus:** clamp _poll_skip to 0 to prevent negative value if pool resets it mid-decrement ([38c80aa](https://github.com/rgon/ncrsDesktop/commit/38c80aad3e40e8abbafdc767380d64e8f75cf15f))
* **nautilus:** log malformed FILE_CHANGES entries; set _poll_skip before clearing _poll_running ([e0f1562](https://github.com/rgon/ncrsDesktop/commit/e0f1562890e2e4d1cca2efb46d29ecf287f57a17))
* **nautilus:** read config as utf-8 and quit nautilus in postinst ([a05fcf7](https://github.com/rgon/ncrsDesktop/commit/a05fcf705984cbe537de0dbde95e2564b820ab51))
* **nautilus:** refuse dev install when the packaged extension copy exists to avoid GObject type collision ([b3ae4f9](https://github.com/rgon/ncrsDesktop/commit/b3ae4f9a5caf76d299e16fbc4b2dc2246b4ea494))
* **net:** retry transient network errors and map timeout/network to ETIMEDOUT/EAGAIN ([bd1e882](https://github.com/rgon/ncrsDesktop/commit/bd1e8826305d7fa48876cf20b9e58f601d4bc410))
* never return empty readdir for deferred dirs, debounce dir cache saves ([bc5bc20](https://github.com/rgon/ncrsDesktop/commit/bc5bc20ca94d0351dc71d9bd940d989c43741e32))
* only prefetch subdirs/thumbnails on first readdir, deduplicate prefetch PROPFINDs ([4f5db18](https://github.com/rgon/ncrsDesktop/commit/4f5db180af9b71d6d573fa1db9dd0363d677f5fe))
* **overlay:** use maximize() so overlay respects taskbar/dock work area ([6d391d9](https://github.com/rgon/ncrsDesktop/commit/6d391d9bd580f6a52d4f7875feae22772e08937e))
* **packaging:** build GUI with custom-protocol so the deb embeds the frontend ([3f8c706](https://github.com/rgon/ncrsDesktop/commit/3f8c706a565a70d5308bcab9d8e8ba092585a601))
* parse statusCode as string to match Nextcloud Passwords API response ([25cc8bc](https://github.com/rgon/ncrsDesktop/commit/25cc8bcdef1ce3ffa5954e5af86f9eb33e3911f2))
* percent-decode paths from remotefs-webdav list_dir results ([84d13c7](https://github.com/rgon/ncrsDesktop/commit/84d13c740af5a84361daad3da230c6972a3de69d))
* percent-decode search result titles and paths ([ff294c1](https://github.com/rgon/ncrsDesktop/commit/ff294c1406a1921236fbea9473cc9b2bbdf26542))
* populate IPC maps from getattr/lookup, serve from file_cache in read, move poll off main thread ([29ae52c](https://github.com/rgon/ncrsDesktop/commit/29ae52cde2db51d2a38fb23461741d10ca15d4a4))
* prevent concurrent PROPFIND race in get_or_list_dir ([5d1a506](https://github.com/rgon/ncrsDesktop/commit/5d1a506648dcb423548c3730557920d2e3e83bfb))
* prevent recursive LOG IPC calls, increase socket timeout and recv buffer ([cd57209](https://github.com/rgon/ncrsDesktop/commit/cd57209e5f1489bb1c677f23c34ae4a19fa57b5e))
* **preview:** avoid Cow allocation in fetch_preview_bytes; preallocate PNG buffer; align probe with daemon params ([8376a3c](https://github.com/rgon/ncrsDesktop/commit/8376a3c85eafd7c1fc15fd93235ad8ce978dd241))
* **preview:** convert NC JPEG preview response to PNG for XDG thumbnail cache ([f19f259](https://github.com/rgon/ncrsDesktop/commit/f19f2599d948b45c481716207dbee010bdeada1b))
* **preview:** evict XDG fail-cache entries when thumbnail is written ([c662680](https://github.com/rgon/ncrsDesktop/commit/c662680414452afd58b19b7000786275522261c5))
* **preview:** guard thumbnail_callback with thumb_inflight to prevent duplicate NC fetches ([6c526bc](https://github.com/rgon/ncrsDesktop/commit/6c526bc7e67370f18e578e4abee19ac9cafaf7b5))
* **preview:** percent-encode file: URIs so XDG thumbnail hashes match Nautilus ([ed1a42d](https://github.com/rgon/ncrsDesktop/commit/ed1a42d796349797052ff15d28f37320fa198c14))
* **preview:** prefetch thumbnails for previewable files lacking server-cached previews ([b09cc6c](https://github.com/rgon/ncrsDesktop/commit/b09cc6c3f8ac1fdf64972f70d8f162ab1ed24193))
* **preview:** throttle on-demand RAW thumbnail fetches to avoid starving FUSE HTTP workers ([8473f2b](https://github.com/rgon/ncrsDesktop/commit/8473f2b5561834a7d88d7f690278ac8d36e345a4))
* **read:** fail copy immediately when uncached file unreachable instead of serving stale cache ([7ec0e7a](https://github.com/rgon/ncrsDesktop/commit/7ec0e7a7e06d9fe5477c5a07cf9199f8c1ad552c))
* reduce Keep Locally concurrency to 2 with yield to avoid Nautilus freeze ([708e4f5](https://github.com/rgon/ncrsDesktop/commit/708e4f547da884bc32a9da3b4eb4539371fd471c))
* reduce thumbnail batch to 4, add 200ms inter-batch and 500ms initial delay ([766fa25](https://github.com/rgon/ncrsDesktop/commit/766fa25b50a23e520ca408d521db543c84063ca5))
* reject empty bearer_token, prevent notification poll from killing other pollers ([f18f696](https://github.com/rgon/ncrsDesktop/commit/f18f6963d45d010945f3840235724315c2138253))
* **release:** annotate workspace version for release-please and unify plugin versions ([d818b7e](https://github.com/rgon/ncrsDesktop/commit/d818b7e79ada55ba356752b2f8f38ee95d8c5a66))
* **release:** use generic extra-file updater for workspace Cargo.toml ([6d0ebb2](https://github.com/rgon/ncrsDesktop/commit/6d0ebb2ee4482e357ee041b4188b523d9c662744))
* **release:** use rust release type so release-please updates workspace Cargo.toml version ([9200368](https://github.com/rgon/ncrsDesktop/commit/920036846d857197608f4755f24b6f74ad45fb43))
* **remount:** unmount active FUSE mount before restarting so mount path changes take effect ([27931d4](https://github.com/rgon/ncrsDesktop/commit/27931d4fca349877e5f8d1896daaeb4c2bf6a632))
* remove dbus reload, let inotify events handle per-file nautilus updates ([fde890d](https://github.com/rgon/ncrsDesktop/commit/fde890d2964964e41a7e9ffd5cdbfd3d6793f92c))
* remove update_file_info stub that blocked async update_file_info_full ([3d32df1](https://github.com/rgon/ncrsDesktop/commit/3d32df1cc9401e5249f26b0f27277331bf4a21ae))
* replace hard cancel with soft self-cancel, limit read throttle to 3 ([4ce0496](https://github.com/rgon/ncrsDesktop/commit/4ce049623652c19feb15f667c47bcd1318351861))
* resolve $plugins alias with absolute path and add plugin loading diagnostics ([a660c06](https://github.com/rgon/ncrsDesktop/commit/a660c06353ea6eb40f06db9246cbde9aa0cc3757))
* restore update_file_info stub, add DETAIL_ASYNC tracing ([ba29455](https://github.com/rgon/ncrsDesktop/commit/ba29455c7a5f68d25980b3d412e7801738bd927c))
* return COMPLETE synchronously for non-mount files to avoid Nautilus async overhead ([0389ab8](https://github.com/rgon/ncrsDesktop/commit/0389ab86025d64c2e282b3dbcafe1ef874a2793d))
* **security:** chmod 0600 config file to protect app password ([5ca3337](https://github.com/rgon/ncrsDesktop/commit/5ca3337ed7df7cb2d897d770698f08fe9a5e370c))
* send self-entry immediately via channel, mark paths dirty after IPC population ([29c5a98](https://github.com/rgon/ncrsDesktop/commit/29c5a98482ae6766332ef2edf502d57303dbfdb4))
* **settings:** add gap between toggle label and checkbox ([a28ad2e](https://github.com/rgon/ncrsDesktop/commit/a28ad2eff626f0b0fa33a8c4418092f9832e233f))
* skip background download for streaming media, show blue emblem during active downloads ([94452e5](https://github.com/rgon/ncrsDesktop/commit/94452e5deb7b5143cbf19c17cecc0128f49bfd9d))
* skip IPC socket queries for files outside ncrs mount point ([eefdd6d](https://github.com/rgon/ncrsDesktop/commit/eefdd6d8d6701f3e3a2b1e025cacb61794ef156d))
* skip zero-byte cached files and clean up failed downloads ([e1f8c4a](https://github.com/rgon/ncrsDesktop/commit/e1f8c4a247a41360a101fc4ac2f2bb948c6a5ff6))
* stop eager full-file download on open, use fileId for preview API ([6c60c37](https://github.com/rgon/ncrsDesktop/commit/6c60c37e92d4a146bcf685dce5d04cbdfb7af5c3))
* suppress notify_push self-notification loop via ETag pre-check ([f5350b1](https://github.com/rgon/ncrsDesktop/commit/f5350b10b9f14a4764c6ed7c5e598b033486c94e))
* suppress self-notify kernel dentry invalidation for freshly-fetched dirs ([e492b37](https://github.com/rgon/ncrsDesktop/commit/e492b37325353ff32925a7f23a449ccc41f0bc9b))
* **sync:** drain in-flight PUT before live DELETE so lock-file create-then-delete doesn't hit 423 Locked ([53f9fd1](https://github.com/rgon/ncrsDesktop/commit/53f9fd1674141942a1ff638234325e93492c7754))
* **sync:** fsync staged bytes before journaling and make journal writes crash-durable ([9b6a533](https://github.com/rgon/ncrsDesktop/commit/9b6a533ef6228a0565da9ade1e0f1f76ba6cd4f8))
* **sync:** hold live MOVE until source PUT drains and treat rename 404 as move-source-gone conflict ([caaf4ba](https://github.com/rgon/ncrsDesktop/commit/caaf4ba4ffb6ed3b7dabfd3e3f490750fe91cade))
* **sync:** never discard local edits on server outage — treat 5xx/timeout/locked as retryable and preserve staged bytes on permanent failure ([8ee1fa1](https://github.com/rgon/ncrsDesktop/commit/8ee1fa1276b89db07b675300a3f22f364e9969f2))
* **sync:** treat live-DELETE 423/5xx as retryable so lock-file deletes don't surface 'resource locked' ([71f365e](https://github.com/rgon/ncrsDesktop/commit/71f365e0a884a7d95f0f78479fbaac4e86f885d1))
* **thumbnailer:** catch OSError from missing exiftool; use or-fallback for XDG_RUNTIME_DIR ([7c42d8d](https://github.com/rgon/ncrsDesktop/commit/7c42d8de732540d7dcc9df8cec9d9f12cf354ee9))
* **thumbnailer:** route by is_remote instead of is_local so slow/absent daemon falls through to exiftool ([881bcf3](https://github.com/rgon/ncrsDesktop/commit/881bcf3894f5d25ba67a533e23b907e05cd70d15))
* **tray:** remove Settings menu item (duplicate of Open ncRS) ([f7277d0](https://github.com/rgon/ncrsDesktop/commit/f7277d066e7d51ed8abd9476861286c5749014fa))
* **typecheck:** resolve all svelte-check errors ([e80c94c](https://github.com/rgon/ncrsDesktop/commit/e80c94cccc630d09167db14b192c51c2cf20f485))
* **ui:** expandable error cards, pointerdown close, ENOSPC retry storm ([9267a5f](https://github.com/rgon/ncrsDesktop/commit/9267a5f4daf3535f1b2b0802e0e45321c53c116c))
* **ui:** fix server label clipping and avatar dropdown overflow ([43cd4c5](https://github.com/rgon/ncrsDesktop/commit/43cd4c5f8555da7093b8e64c7bb0eae780503550))
* **ui:** move [@const](https://github.com/const) tags to be direct children of {#if} block ([4efb8ec](https://github.com/rgon/ncrsDesktop/commit/4efb8ecbff64f133c3f64d6a2465d3794d523123))
* **ui:** register event listeners before loading initial state to avoid notification race ([8219529](https://github.com/rgon/ncrsDesktop/commit/821952947de27a10a7c0ede88c6210cd9f4da642))
* **ui:** show 'Log in' button for auth errors instead of 'Remount' ([3144987](https://github.com/rgon/ncrsDesktop/commit/31449872c6e1295a161e284447713eae0b746f77))
* **ui:** show FUSE error message in sync label instead of invisible alert span ([bd973bc](https://github.com/rgon/ncrsDesktop/commit/bd973bc2ee68afbe717b9c62fea4fb7116ffd455))
* unmount FUSE on quit, handle existing mount point, and sync tray state on all transitions ([4a853e3](https://github.com/rgon/ncrsDesktop/commit/4a853e3109d4c6ab111546b11ee1d28d981472e4))
* update mount_ncfs call signature and harden MKCOL/DELETE ops ([d745ae9](https://github.com/rgon/ncrsDesktop/commit/d745ae9b2e9de00cc9dc2e16776d02f4b928c839))
* use download arrow emblem for partial-download folders ([b93a90d](https://github.com/rgon/ncrsDesktop/commit/b93a90d5ff67bdd0fbc870b2978ee3b2033558a1))
* use Nautilus.FileInfo.lookup for invalidation instead of storing stale GObject refs ([85b4aef](https://github.com/rgon/ncrsDesktop/commit/85b4aef908436acf8629342f6fdad067dedbec1f))
* use shared buffer with condvar for incremental read-ahead streaming ([1ac65bc](https://github.com/rgon/ncrsDesktop/commit/1ac65bccec0f3d020083f36f0899a98cec44096e))
* use sync update_file_info with background cache for non-blocking NC columns ([d2efdd3](https://github.com/rgon/ncrsDesktop/commit/d2efdd3ef4ad2b208d3613b037c870365c42fa24))
* use synchronous DETAIL IPC in update_file_info for immediate NC columns ([12860a0](https://github.com/rgon/ncrsDesktop/commit/12860a04086ffc9aa53ee397410e96ff0095506a))
* wait for PROPFIND completion instead of 2s timeout, increase IPC socket timeout ([96b4fa7](https://github.com/rgon/ncrsDesktop/commit/96b4fa7f245e8631fed8bde6910628e2ec9333d7))
* **webdav:** use Overwrite: T in MOVE so rename atomically replaces existing destinations: WebDAV move had different behaviour to POSIX move. ([d7ccaa3](https://github.com/rgon/ncrsDesktop/commit/d7ccaa3459ae526614d0c9e29f5b08b0ef46e731))
* wrap all Nautilus extension callbacks in try/except to prevent crashes ([fa678b8](https://github.com/rgon/ncrsDesktop/commit/fa678b8fa2af962322ec9778cc84ca1a95bd6ea5))


### Performance Improvements

* add prefetch_throttle so aggressive_prefetch doesn't compete with READDIR ([213a34d](https://github.com/rgon/ncrsDesktop/commit/213a34dffdb9c9616e2e38d52ba890f704ec3cb2))
* add throughput metrics to range read logging ([82bb0dd](https://github.com/rgon/ncrsDesktop/commit/82bb0dd7ebf43425eb4613b129c31acb40b68fcc))
* **build:** merge two cargo build passes into one to avoid recompiling shared deps ([51b93b2](https://github.com/rgon/ncrsDesktop/commit/51b93b2ac62b536f5fe860e19694415571e0a1df))
* cap thumbnail and subdirectory prefetching for directories &gt;200 entries ([01f2a33](https://github.com/rgon/ncrsDesktop/commit/01f2a33c9a3e6964a49cb10a75ee4e9a89fa264b))
* chain prefetch one level deeper to eliminate pause between traversal waves ([85061be](https://github.com/rgon/ncrsDesktop/commit/85061be28a5ff20279f4a633bcc46f29a119540e))
* **ci:** unit test against release build to avoid re-building both versions in CI ([ab82031](https://github.com/rgon/ncrsDesktop/commit/ab82031bda898e0c2da3af83397d97cd83af1496))
* fix condvar wake + suppress proactive_refresh during traversal ([b153765](https://github.com/rgon/ncrsDesktop/commit/b1537658e0460a7bc2c8ea585b023ac3e3f073d0))
* **fuse:** deduplicate concurrent thumbnail-prefetch threads per directory ([861178d](https://github.com/rgon/ncrsDesktop/commit/861178d061c8e324f42488e0bca599a80f400bf6))
* **fuse:** defer O(n) map retain() calls to after reply.ok() in readdir ([a85c2dd](https://github.com/rgon/ncrsDesktop/commit/a85c2dd7bd4c134c429d629e4283ec75c83d499a))
* **fuse:** intercept GLib MIME detection opens with synthetic magic bytes ([08e5673](https://github.com/rgon/ncrsDesktop/commit/08e5673e7ddf7c49f714be670db0b5cbbf190150))
* **fuse:** prefetch small files on open() to parallelise MIME magic-byte detection ([9cc6505](https://github.com/rgon/ncrsDesktop/commit/9cc65059f614f0b03847445a25676bf44a923bd4))
* **fuse:** raise attr TTL to 30 s to avoid per-second kernel re-queries ([8fc19cf](https://github.com/rgon/ncrsDesktop/commit/8fc19cfc0ccf3057b4b8e8cb7b41a92a8e417293))
* **fuse:** release cache lock before reply.add() loop to unblock concurrent getattr ([fdb907a](https://github.com/rgon/ncrsDesktop/commit/fdb907aa2ff00dce64b3eeac39ddc7edeb683c70))
* **fuse:** release cache lock before scanning dir entries in getattr and lookup ([52cf175](https://github.com/rgon/ncrsDesktop/commit/52cf175b68e7308c01224ca64077d5eefe442e44))
* **fuse:** short-circuit second metadata() stat when kept_path already matches ([3769185](https://github.com/rgon/ncrsDesktop/commit/37691851b3affb90558a3e2a4e10422d50980f18))
* **gui:** apply pushed snapshots per-field so only changed fields re-parse and re-emit ([3ad2cef](https://github.com/rgon/ncrsDesktop/commit/3ad2cef17fcda06af861a60cc55dc8cf15468760))
* **gui:** end idle tray CPU by subscribing to daemon pushes and freeing the webview on close ([4fe90ab](https://github.com/rgon/ncrsDesktop/commit/4fe90abcc4e64f6cff1c171c235f647e9ae843f8))
* **gui:** use jemalloc to bound RSS from read-ahead buffer churn ([89e02e6](https://github.com/rgon/ncrsDesktop/commit/89e02e6aeeac95ef4ab2411cb81216e2d1e1930c))
* increase read-ahead to 64MB and add background prefetching ([7869377](https://github.com/rgon/ncrsDesktop/commit/7869377f63da96ec83426278416761a48c757ef5))
* increase thumb prefetch concurrency (batch 32, cap 200) and fix stray lock unwrap ([f58528e](https://github.com/rgon/ncrsDesktop/commit/f58528e5939f0f56102837a695496dfc75e7669a))
* **ipc:** add ChildrenMap index for O(dir_size) detail/status lookups ([2241bf5](https://github.com/rgon/ncrsDesktop/commit/2241bf5488b6ea84b1bbab27de8ad55440518592))
* **ipc:** aggregate DETAILDIR directory statuses in one pass instead of O(subdirs*N) scans ([0b7fe71](https://github.com/rgon/ncrsDesktop/commit/0b7fe71f5ea3f6075a04d5bd1c33fb81342e1bce))
* **ipc:** cache the daemon state snapshot's journal JSON by version and skip rebuilding unchanged ticks ([e2c994e](https://github.com/rgon/ncrsDesktop/commit/e2c994ed044d1d383bb71ed83c52487c79ca58a9))
* **ipc:** convert StatusMap/FileDetailMap/SharedSet/FileIdMap to RwLock to fix DETAILDIR starvation ([da95012](https://github.com/rgon/ncrsDesktop/commit/da950122f6cded587c9809882a9beda0ba424535))
* **ipc:** demote DETAIL log to debug, fix thumbnailer crash on malformed JPEG ([a0c247a](https://github.com/rgon/ncrsDesktop/commit/a0c247aad1ba26fc008c4b26f06ac750e32af86b))
* **ipc:** drop status/detail locks before joining DETAILDIR reply so huge dirs don't stall FUSE ([7632fae](https://github.com/rgon/ncrsDesktop/commit/7632fae39a72a552de5278c3b21da6a8c4ece92a))
* **ipc:** pre-populate detail/shared/fileid maps before readdir reply.ok() to fix race with Nautilus extension DETAIL queries ([d246cf8](https://github.com/rgon/ncrsDesktop/commit/d246cf85c9b6be8b136dc478e4bebfaa0d6cd1fa))
* make lookup/getattr cache-only, no PROPFIND triggered by Nautilus file scanning ([9ac60cf](https://github.com/rgon/ncrsDesktop/commit/9ac60cf1761eb931a575fc9ca1ff574457f01bf1))
* **nautilus:** avoid FUSE utimes upcall for M-type FILE_CHANGES events ([396829b](https://github.com/rgon/ncrsDesktop/commit/396829b0249342a85a27e2cec43dcf44057fbf82))
* **nautilus:** bound the directory metadata cache and precompute the mount prefix ([d44e126](https://github.com/rgon/ncrsDesktop/commit/d44e1266619e3219139a22f277690a0f3046ffa2))
* **nautilus:** cap pending invalidations and split pdf thumbnail throttle ([64f2e98](https://github.com/rgon/ncrsDesktop/commit/64f2e98bd7d87670c018a4f11171c2becc7c787f))
* **nautilus:** guard poll loop against backpressure when daemon is slow ([30c32c0](https://github.com/rgon/ncrsDesktop/commit/30c32c044bfbff679bfd87f6f142d8e53ed7bcc5))
* **nautilus:** make update_file_info async via update_file_info_full + IN_PROGRESS ([bd1eaa8](https://github.com/rgon/ncrsDesktop/commit/bd1eaa82771e431758a4c4665b96b69b8ae528ca))
* **nautilus:** patch changed cache entries in place instead of refetching the whole directory on notify_push updates ([049e167](https://github.com/rgon/ncrsDesktop/commit/049e167b3fb69e0d175165bd38af8793749c31f1))
* **nautilus:** remove per-file STATUS queries from get_file_items GTK thread ([2dcc8fb](https://github.com/rgon/ncrsDesktop/commit/2dcc8fb21869a6411c5f129d4e17544fae590a02))
* **nautilus:** repaint only requested children after a fetch to avoid O(dir) invalidation on the main thread ([608625c](https://github.com/rgon/ncrsDesktop/commit/608625c9cce20321a1250d256ef075023045375f))
* **nautilus:** replace blocking _poll_keep_done loop with GLib timeout ([4a937c0](https://github.com/rgon/ncrsDesktop/commit/4a937c0821b9182e3981bfe38c438f56695ffa9d))
* **nautilus:** resolve path via single get_uri() and read cache lock-free in the per-file hot path ([d181217](https://github.com/rgon/ncrsDesktop/commit/d181217b5f047f7a9a0bf90ffc05e4fa7595fa1b))
* **nautilus:** serve file info synchronously from a per-directory cache to fix slow listings and handle=(nil) spam ([e11853d](https://github.com/rgon/ncrsDesktop/commit/e11853d8f00c5d138eed1561a874dcef424d903c))
* **nautilus:** skip empty file attributes and memoize perms to cut per-file GObject calls ([f389cb4](https://github.com/rgon/ncrsDesktop/commit/f389cb41cda0b57b5521ae71b161d12c20a2fac2))
* **nautilus:** warm dir cache synchronously on cold open to kill O(N) repaint storm ([eaf3414](https://github.com/rgon/ncrsDesktop/commit/eaf34144b43a2022d92296274e23aafdebf31473))
* populate IPC maps only once per directory and cap CHANGES to 500 paths ([19147fd](https://github.com/rgon/ncrsDesktop/commit/19147fda83cf627c5e73701327dd2a2e5a64b908))
* prefetch child dirs concurrently on readdir using pending_dirs ([648b591](https://github.com/rgon/ncrsDesktop/commit/648b591720aebf2eb32a5d41963ede2411f0bdcc))
* **preview:** increase thumbnail throughput; add fetch/convert timing logs ([738fe16](https://github.com/rgon/ncrsDesktop/commit/738fe167957deac999fc3441cd961b98b73064d0))
* **preview:** write thumbnail via tmp+rename to prevent concurrent corruption ([8e5227d](https://github.com/rgon/ncrsDesktop/commit/8e5227d7784743e8e029299f232670054268462a))
* reduce prefetch contention and thumbnail size for faster directory loads ([2034acc](https://github.com/rgon/ncrsDesktop/commit/2034acc0be6de714e9242191b3a2021a4e745e40))
* reduce thumbnail batch size and gate on active streams ([f8c10e2](https://github.com/rgon/ncrsDesktop/commit/f8c10e27a704a342068771a1589dfea183aa1e24))
* separate read throttle, increase read pool to 8, enable tcp_nodelay ([d430848](https://github.com/rgon/ncrsDesktop/commit/d43084829dfeb3b21f73cfc3cf9589938e1b95db))
* share HTTP client across requests and parallelize thumbnail prefetch ([e8d1fb0](https://github.com/rgon/ncrsDesktop/commit/e8d1fb0865119b5649d9a5e5303714a9c212ba90))
* short-circuit readdir continuation pages from cache ([78395d7](https://github.com/rgon/ncrsDesktop/commit/78395d783f504395f60881fb1e98b5d105f4c5eb))
* skip read throttle for small reads without read-ahead ([b9e2afb](https://github.com/rgon/ncrsDesktop/commit/b9e2afbf43e4e50e8a0c091643cec5e9cd667a80))
* split IPC population into per-map short locks to reduce FUSE contention ([62e00b4](https://github.com/rgon/ncrsDesktop/commit/62e00b401fa318b1723f4cb46d84f532b96532f2))
* stop flooding dirty set on notify_push file change events ([5b50437](https://github.com/rgon/ncrsDesktop/commit/5b504370b144935b8438e42b5adbade2bbfa1eb6))
* stream range reads to reply with first bytes immediately ([f57c34c](https://github.com/rgon/ncrsDesktop/commit/f57c34c256c2051a928e37e25b10da0295f12f39))
* stream XML parsing directly from HTTP response instead of buffering ([c79644c](https://github.com/rgon/ncrsDesktop/commit/c79644cd2b4986832404e73268f0197709c95fa7))
* switch back to async update_file_info_full with 32-worker pool for parallel DETAIL queries ([b988fec](https://github.com/rgon/ncrsDesktop/commit/b988fece1b1300dd606d53036c465c655b167ba3))
* switch Nautilus DETAIL queries to synchronous IPC, eliminate thread pool bottleneck ([f1f8691](https://github.com/rgon/ncrsDesktop/commit/f1f86912217ba142e149ba76c84878aa7ac80f5d))
* **thumbnailer,extension:** fix hang risks, spurious NC ops, and idle poll overhead ([836cae3](https://github.com/rgon/ncrsDesktop/commit/836cae308e7d781ff41240d37ae1ec3b002bbe1b))
* **upload:** stream PUT from staging file instead of loading into RAM ([07dc81a](https://github.com/rgon/ncrsDesktop/commit/07dc81a47b8ec5b4369fd0610e6e017e9727362d))
* use Arc&lt;Vec&lt;DavEntry&gt;&gt; in dir cache to eliminate O(N) clones per FUSE call ([9796f99](https://github.com/rgon/ncrsDesktop/commit/9796f99f495909c475ae3bed99289ea29c4b10ec))
* WebDAV connection pool and speculative subdirectory prefetching ([adadcc6](https://github.com/rgon/ncrsDesktop/commit/adadcc68e06eae2ab55219feb818d252b142c050))

## [0.1.50](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.49...ncrs-v0.1.50) (2026-08-10)


### Bug Fixes

* **core:** adopt orphaned mount-point writes instead of refusing to remount ([fdc45cd](https://github.com/rgon/ncrsDesktop/commit/fdc45cd186afa0a8fd8c2ac215528f196a5c7dfc))
* **core:** broaden is_transient_network_err to prevent FUSE EIO on transient network failures ([6f9ff75](https://github.com/rgon/ncrsDesktop/commit/6f9ff75a5cd15b69b95ef5435399852059d93cac))
* **core:** clear deleting guard on journal replay ServerError and MAX_ATTEMPTS paths ([f5e02a4](https://github.com/rgon/ncrsDesktop/commit/f5e02a4c1838ad4ae3eba041c8f19fc32912f623))
* **core:** prevent racing PROPFIND from re-surfacing in-flight deleted files and dirs ([a441d36](https://github.com/rgon/ncrsDesktop/commit/a441d361ec96db11b45cef0e1d8f00b931b59972))
* **e2e:** install procps in e2e container for pgrep in scenario 19 ([c41482b](https://github.com/rgon/ncrsDesktop/commit/c41482b8d7edbcc5ecfa98b069f44216566d23dd))
* **e2e:** mkdir adopt dir on real fs after lazy unmount so orphaned write lands ([788dd0f](https://github.com/rgon/ncrsDesktop/commit/788dd0f1b661d80d31db74f7899fc6d09b9bb979))
* **gui:** stop lazy-detaching busy mounts to avoid orphaned writes ([7c283d1](https://github.com/rgon/ncrsDesktop/commit/7c283d1d337910baf311fac26cb2edc312d27238))

## [0.1.49](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.48...ncrs-v0.1.49) (2026-07-28)


### Features

* --auto-keep-locally-modified-files flag controls post-upload emblem and local copy ([ef4fba1](https://github.com/rgon/ncrsDesktop/commit/ef4fba1b123b0ad3faa9daaec27ced24a0c56936))
* add 'View in Nextcloud Web' right-click menu via WEBURL IPC command ([12b64cf](https://github.com/rgon/ncrsDesktop/commit/12b64cfd0b9af16a25a196510adcaced66552d5b))
* add bearer_token and auth_command to config ([9cbbc23](https://github.com/rgon/ncrsDesktop/commit/9cbbc2394b0cbf3c92a9d8075158853141513c22))
* add cache cleanup with configurable max size and auto-purge by age ([07b3e8e](https://github.com/rgon/ncrsDesktop/commit/07b3e8e62b6ae4e923abc279a8875f9d7044b503))
* add CHANGES invalidation tracing to Nautilus extension ([69bfa2f](https://github.com/rgon/ncrsDesktop/commit/69bfa2f2fe7614b16fdd244e2aa22c598dba242d))
* add CHANGES IPC command for live emblem refresh after Keep Locally ([1ea8f11](https://github.com/rgon/ncrsDesktop/commit/1ea8f11deec2d9c70197135e39598829bb97c1f0))
* add CLI argument parsing with --offline, --config, and --mount-point flags ([1409a80](https://github.com/rgon/ncrsDesktop/commit/1409a800e2227112ad729f86fc9d86cea0b2b8d6))
* add configurable cache cleanup interval (default 1h) ([f0dfe4e](https://github.com/rgon/ncrsDesktop/commit/f0dfe4eb7b68d6f23561cf1a97e71f477677ca1e))
* add ConflictsView and ErrorsView components ([c3d0a36](https://github.com/rgon/ncrsDesktop/commit/c3d0a3681fc9088798e14af33049e0700c4c9ad4))
* add Credentials auth abstraction for basic and bearer token support ([61d6488](https://github.com/rgon/ncrsDesktop/commit/61d6488015b25078a34ee38d976b2237125f6dcf))
* add deb build script ([8169ea6](https://github.com/rgon/ncrsDesktop/commit/8169ea68646ba68cb717a02f721262113c402e80))
* add exclude_folders config to hide folders from FUSE mount ([78ae3cc](https://github.com/rgon/ncrsDesktop/commit/78ae3ccdd0109dc051465059907aa9d9e3e1ae95))
* add filename validation module matching Nextcloud server rules ([6bcf3d7](https://github.com/rgon/ncrsDesktop/commit/6bcf3d7cbd66a005edd5d2d07fb0710730443980))
* add frontend plugin registry and plugins view ([549cefe](https://github.com/rgon/ncrsDesktop/commit/549cefec842dbfe7e6de870345b41c4cefa7d578))
* add fuse_notify and mutation_journal modules ([07adcf2](https://github.com/rgon/ncrsDesktop/commit/07adcf2237c526cbe664dee679ae352884120315))
* add GNOME Shell search provider for server-side Nextcloud search ([29e4595](https://github.com/rgon/ncrsDesktop/commit/29e4595540f8dfe53224a961f15434b51960fbae))
* add info-level tracing across FUSE, IPC, PROPFIND and Nautilus extension ([1d77751](https://github.com/rgon/ncrsDesktop/commit/1d7775197b86b63c23a1f666e200cb7405b2ecbe))
* add keep_paths config to auto-keep remote paths on startup ([4cdf9a8](https://github.com/rgon/ncrsDesktop/commit/4cdf9a870a3fc55638592cfb61095e7a186447ae))
* add logging ([5fe7983](https://github.com/rgon/ncrsDesktop/commit/5fe79837e2248cf75d3265e77873ba18a7af0a9e))
* add Nautilus columns for sync, sharing, permissions, owner, and size ([357a3fd](https://github.com/rgon/ncrsDesktop/commit/357a3fd3cb391c550986c04d2f22285a7f72b32f))
* add nc_passwords plugin crate with API client ([3d061f3](https://github.com/rgon/ncrsDesktop/commit/3d061f38f0a2b37e232003e5dcbd274fd68963c3))
* add nc:// URI handler for Nextcloud Edit Locally support ([fa604b4](https://github.com/rgon/ncrsDesktop/commit/fa604b48ec945ad6e98b608ce520a8e3fa6dac50))
* add ncrs_plugin shared crate with plugin trait ([8a31352](https://github.com/rgon/ncrsDesktop/commit/8a31352e1bfd1a4840d204c585f043a89a431371))
* add Nextcloud Notify Push WebSocket client for real-time cache invalidation ([5ca11cf](https://github.com/rgon/ncrsDesktop/commit/5ca11cf565f39f8e5c7c48e3952391d69660a633))
* add Nextcloud OCS notifications API to ncrs_core ([e7af4e9](https://github.com/rgon/ncrsDesktop/commit/e7af4e9ea9f1544570124f43fde07920af717644))
* add Nextcloud unified search API to ncrs_core ([b538ea5](https://github.com/rgon/ncrsDesktop/commit/b538ea5b60b3097a896426722d7ee579e9092ad6))
* add oc:permissions and oc:fileid to PROPFIND properties ([d0017d2](https://github.com/rgon/ncrsDesktop/commit/d0017d25353e115bfe246aadefd7f91b4279614f))
* add offline resilience with connectivity monitor and cache-only fallback ([747b9c9](https://github.com/rgon/ncrsDesktop/commit/747b9c98416c1c948bf6c8693032a4c6dfb96547))
* add PasswordsView frontend and register in plugin UI ([07e33de](https://github.com/rgon/ncrsDesktop/commit/07e33deef1f862b1517db0b54caee575ccbe2c17))
* add plugin registry and tray integration ([06a5cf9](https://github.com/rgon/ncrsDesktop/commit/06a5cf96bfdba4ec3f17feb68401d915e80fd4b1))
* add provider type filtering to search ([d77319f](https://github.com/rgon/ncrsDesktop/commit/d77319f68c9f1ab9a64c3c1a99d3ba1bfa02d8e5))
* add resolve_fileids() WebDAV SEARCH by fileid ([a13eaa0](https://github.com/rgon/ncrsDesktop/commit/a13eaa04566741592fd35a62566218b2c6e2e67b))
* add SEARCH IPC command and Nautilus search dialog for Nextcloud ([38e6019](https://github.com/rgon/ncrsDesktop/commit/38e6019875bcdab86e2e0ee0cf1050338951a043))
* add search_nextcloud Tauri command with parallel provider queries ([3aee79a](https://github.com/rgon/ncrsDesktop/commit/3aee79ab112108f695895663a80d3f011d4c492e))
* add SearchView component with debounced NC unified search ([f41eb99](https://github.com/rgon/ncrsDesktop/commit/f41eb993c9551d21e98e241ef65a7f4eab9c9f22))
* add systemd user service and deb packaging metadata ([7b7333b](https://github.com/rgon/ncrsDesktop/commit/7b7333b05ba2d90bc27d1967f5df75e98b677377))
* add WebDAV write operations with FUSE callbacks and ETag conflict resolution ([bdc122a](https://github.com/rgon/ncrsDesktop/commit/bdc122afd63032d03321107a0c2ff11ec42d15ce))
* add window-context detection for auto-suggesting passwords based on active browser tab ([ea1c5ec](https://github.com/rgon/ncrsDesktop/commit/ea1c5ec0e13f3aba2d5622229d7121ad7d74aa4b))
* **auth:** Nextcloud Login Flow v2 — trigger when password missing ([f8293e7](https://github.com/rgon/ncrsDesktop/commit/f8293e76f4337915c3183c547e1d5ffe72b819f3))
* **auth:** system keyring for app password; warn on insecure config perms ([35e074b](https://github.com/rgon/ncrsDesktop/commit/35e074b5377e9ed53a971ba91fc542fb9795c35d))
* auto-create mountpoint if doesn't exist ([9de011b](https://github.com/rgon/ncrsDesktop/commit/9de011bff871f1a25ca7191c8493a71e0cdc9666))
* background file caching on open, keep-locally IPC+menu, fix emblems, add debug logging ([f792818](https://github.com/rgon/ncrsDesktop/commit/f79281806ebbe772fb60f5940303276bacc7413e))
* background notification polling and dismiss/get Tauri commands ([9ad4fb5](https://github.com/rgon/ncrsDesktop/commit/9ad4fb529ec9d8ea44258faf2171fe3d44b22e8c))
* cache directory ls and update with notify-push ([415a87f](https://github.com/rgon/ncrsDesktop/commit/415a87fd0ef0208c544e90eb9bbc13a12cba7e04))
* cache streaming reads to disk when full file is covered, with configurable read-ahead ([54b441d](https://github.com/rgon/ncrsDesktop/commit/54b441d9f1c707ab44d89d5c8ddce2d06dc19e5c))
* **calendar:** add nc_calendar plugin with GOA/CalDAV auto-registration ([e58b41e](https://github.com/rgon/ncrsDesktop/commit/e58b41e476d4c232a6a2db4264ceca240ebe8eaa))
* **ci:** build the deb via build-deb.sh and verify it with test-deb.sh ([6eadd86](https://github.com/rgon/ncrsDesktop/commit/6eadd86600697fbc195d7cb042509c786ca5683a))
* **cli:** add --url/--username/--password overrides and remove http3 probe ([a24a1f0](https://github.com/rgon/ncrsDesktop/commit/a24a1f0e8c02a669cc322ba9cbfb4cb8f99e4e17))
* **config:** auto-normalize DAV URL from any user-supplied format ([1f582b6](https://github.com/rgon/ncrsDesktop/commit/1f582b6ba38341d6b439ac5f7a50852754409db0))
* **config:** enable http3 by default; add explanatory comments to advanced settings ([4b34dbc](https://github.com/rgon/ncrsDesktop/commit/4b34dbc3a33bfefb601c0313ea63d4501648848a))
* **core:** add --print-default-config flag and explicit ncrs bin target ([4b9dd9d](https://github.com/rgon/ncrsDesktop/commit/4b9dd9ddad8906915c21ebf4f5eb90c5694803aa))
* **core:** add STATE, PAUSE and RESUME IPC verbs for external clients ([3b0bb03](https://github.com/rgon/ncrsDesktop/commit/3b0bb034f761c1bf4e6981ec13e9872bf4065ae3))
* deferred subdirectory readdir, partial-download icons, evict command, cache recovery, and fix web URLs ([a4c2a2b](https://github.com/rgon/ncrsDesktop/commit/a4c2a2b91dfcc91a6a866ee3fe3f10da1046db5e))
* derive FUSE mode bits from Nextcloud oc:permissions flags ([32f7be3](https://github.com/rgon/ncrsDesktop/commit/32f7be37b623cd87bed5ccd34eccaca9652012a7))
* derive Serialize/Deserialize for DavEntry for cache persistence ([7f8bc65](https://github.com/rgon/ncrsDesktop/commit/7f8bc65c9ea3f1e12ea5f06c5a0dcfa55f64bb98))
* detect shared files via oc:share-types and show shared emblem in Nautilus ([22d65df](https://github.com/rgon/ncrsDesktop/commit/22d65df9bbbcb7eae2e80e4e8cee1e29538c0f64))
* dir cache persistence, sync reads, HTTP/3 client, child prefetch batching ([1ced20d](https://github.com/rgon/ncrsDesktop/commit/1ced20d4187746245546bb379d58b47a00466aaa))
* expose error_log, transfer_map, and journal through Tauri state and commands ([ab5a4e4](https://github.com/rgon/ncrsDesktop/commit/ab5a4e459c60275c033c5ee4b99fa1c2f5cd266c))
* extend SyncProgressView with transfers and error indicators ([609512d](https://github.com/rgon/ncrsDesktop/commit/609512dac488fa72c96e864e6eaaab76c8d359c6))
* **fuse:** auto-delete stale GIO atomic-write temps from server on readdir ([e02bbfd](https://github.com/rgon/ncrsDesktop/commit/e02bbfd80fd5a2a05e5c75ccef51604a002b6e3e))
* **fuse:** implement readdirplus to bundle entry attributes and cut per-file getattr ([1ef19a9](https://github.com/rgon/ncrsDesktop/commit/1ef19a9b166637854471cf3140f890fc799fd464))
* **fuse:** serve MIME type via user.xdg.mime.type xattr to skip GIO content sniffing ([53a1f18](https://github.com/rgon/ncrsDesktop/commit/53a1f18407e7c839cc56318b5a6fad119b263a5e))
* ghost entries + inotify events for all FUSE ops with Nautilus DBus reload ([8003ed4](https://github.com/rgon/ncrsDesktop/commit/8003ed4e8780dfd6ca77de0003e7f33c4e5e0c15))
* **gui:** add description hints to advanced settings toggles ([a129dcb](https://github.com/rgon/ncrsDesktop/commit/a129dcbc4b447653e11b97f4919e37dd0d860ffc))
* **gui:** add purge local cache action that re-downloads fresh copies while preserving unsynced edits ([c7225d3](https://github.com/rgon/ncrsDesktop/commit/c7225d3736e70b30445dd85b717a92bb59fe38c0))
* **gui:** add Remount button to settings footer ([3838009](https://github.com/rgon/ncrsDesktop/commit/38380091e36837c784b5a39bdf6ac86cc73b9741))
* **gui:** add settings view with yaml config editing and version indicator ([63b3154](https://github.com/rgon/ncrsDesktop/commit/63b315440767e394c001b970a35a8c121296188c))
* **gui:** attach to a running daemon over IPC instead of mounting a second time ([0f9c804](https://github.com/rgon/ncrsDesktop/commit/0f9c804a64bfa21507af1ef52ab641d54afddc09))
* **gui:** left-click tray icon opens window, right-click shows menu ([09d9295](https://github.com/rgon/ncrsDesktop/commit/09d92952867910e1d16d69c87ebc33a6887fbf64))
* **gui:** mirror journal and conflicts from daemon in attach mode ([6e43466](https://github.com/rgon/ncrsDesktop/commit/6e43466534b63a690d65732d401b0638369af321))
* handle FUSE unmount event with shutdown flag, GUI remount ([e6238f8](https://github.com/rgon/ncrsDesktop/commit/e6238f8764f1191a9e191837b853c149971ca4ac))
* **hpb:** add degraded state and orange tray icon for notify_push failures ([0cfa568](https://github.com/rgon/ncrsDesktop/commit/0cfa568196cc4aedf7c725cdb19974e4b0c5896c))
* HTTP Range partial reads with 2MB read-ahead buffer for streaming ([9a10ebc](https://github.com/rgon/ncrsDesktop/commit/9a10ebcbfd424280a4b5f86bd65fc90f760f3bb1))
* **http:** probe HTTP/3 at startup, fall back to HTTP/2 if QUIC unavailable ([8adc9b2](https://github.com/rgon/ncrsDesktop/commit/8adc9b22765a3b51f5449f33458c333b57ac5b1e))
* implement chunked uploads for files larger than 10MB ([a215449](https://github.com/rgon/ncrsDesktop/commit/a2154494fa9295e796d54d318efda65103d31de3))
* implement pause sync via shared paused flag in core and GUI ([a8f3134](https://github.com/rgon/ncrsDesktop/commit/a8f31347810fe9413efc0e6abfa84118f408fa36))
* incremental readdir with channel-based streaming for cold cache ([01c3736](https://github.com/rgon/ncrsDesktop/commit/01c37366d03483fc841e6bc86e55a409adc1f32a))
* **ipc:** add DETAILDIR batch command for directory metadata ([f15d22d](https://github.com/rgon/ncrsDesktop/commit/f15d22d46fb7720265b40237d3c56ea5f5c8dce8))
* **ipc:** add DETAILDIR to batch a directory's child metadata into one reply ([e1eb509](https://github.com/rgon/ncrsDesktop/commit/e1eb509cc744703b1471c691bdb4a5a99462d90c))
* **ipc:** add VERSION handshake so daemon/extension protocol mismatch is logged ([8e13f37](https://github.com/rgon/ncrsDesktop/commit/8e13f37b93bd19725eb183b8bab709d9da1050cf))
* **ipc:** push daemon state to subscribers via a SUBSCRIBE verb so the GUI stops polling ([28efecd](https://github.com/rgon/ncrsDesktop/commit/28efecdc911ecb58ab5c72f06d01364a196a355f))
* **issues:** add Clear all button for conflicts in the warnings tab ([4a49942](https://github.com/rgon/ncrsDesktop/commit/4a499429f66362f0da9ec77f3cfca26b44166f3e))
* **issues:** add per-item error dismiss button ([fe5fde9](https://github.com/rgon/ncrsDesktop/commit/fe5fde94a559e80593219c983610c65e6795c343))
* **logging:** replace env_logger with tauri-plugin-log to surface warnings in DevTools ([fd341e9](https://github.com/rgon/ncrsDesktop/commit/fd341e90918222f406362113fd7422f57bfe2198))
* **logging:** wire @tauri-apps/plugin-log JS package and attachConsole for DevTools output ([9eff244](https://github.com/rgon/ncrsDesktop/commit/9eff2442d0bbc62aeabcaec7f70908b5e1df9328))
* **login:** pre-fill server URL from existing config ([bd7d003](https://github.com/rgon/ncrsDesktop/commit/bd7d003ed4c7ac9a7ad14870dcb79561f8751c49))
* nautilus extension for file sync status emblems ([b63b021](https://github.com/rgon/ncrsDesktop/commit/b63b0217d494079b0ba14a5fffe8f926261d6573))
* open file results in file browser with view-online button ([d844d80](https://github.com/rgon/ncrsDesktop/commit/d844d8013f340d4d47f2087169330d0673b7af97))
* **packaging:** add AppStream metainfo with OARS rating and hardware hints ([880918b](https://github.com/rgon/ncrsDesktop/commit/880918bb77e1e5c5ba7975b106275525f6c4c2aa))
* **packaging:** ship GUI desktop entry, icons, autostart and example config in the deb ([e399413](https://github.com/rgon/ncrsDesktop/commit/e39941328fdf3657952b469d35c9166712a6bfb5))
* populate IPC detail for directories themselves via PROPFIND self-entry ([b7b0bb8](https://github.com/rgon/ncrsDesktop/commit/b7b0bb80ee39ba4fc379cf5fd3e850a15cb9ad17))
* prefetch NC preview thumbnails into XDG cache on directory listing ([16ed616](https://github.com/rgon/ncrsDesktop/commit/16ed6168a2532018df6d89bbc681aac276b09514))
* **preview:** fetch thumbnails for RAW camera formats regardless of has_preview flag ([98a92cf](https://github.com/rgon/ncrsDesktop/commit/98a92cf03e6bcd3b0c5853d8a92d2bf189bf5e1c))
* **preview:** touch FUSE atime after thumbnail write to auto-refresh Nautilus ([5ccd830](https://github.com/rgon/ncrsDesktop/commit/5ccd830948ed9730dcdb0ef172acb0f68ebeff3c))
* proactively propfind invalidated etags of /* at boot ([18a9e2e](https://github.com/rgon/ncrsDesktop/commit/18a9e2eedf9e1d1a0cc62ef85bb64f257af22a49))
* raw PROPFIND with NC properties (has-preview, etag, oc:size), replace remotefs for listings ([c78fd46](https://github.com/rgon/ncrsDesktop/commit/c78fd468d0ef57aa3a0cde2423cd2a204a9da4da))
* redesign PasswordsView as quick-access popup with click-to-copy ([6a28378](https://github.com/rgon/ncrsDesktop/commit/6a283789f1e2146f429a9dafcfd67afbcf92a8d8))
* register nc_passwords plugin in tauri backend ([853a457](https://github.com/rgon/ncrsDesktop/commit/853a45791512e0b1ca16975eaaeb7a824e39063b))
* replace dummy notifications with real NC API data, avatar, and dismiss ([ebc156b](https://github.com/rgon/ncrsDesktop/commit/ebc156b11cff81e82d8ef06833a501d575df3c60))
* replace stub 'Add account' with working Log out button ([c16bf42](https://github.com/rgon/ncrsDesktop/commit/c16bf42bab588e16e099f484e389e65d686a9548))
* separate kept and cached files into distinct directories with per-file status ([9157a48](https://github.com/rgon/ncrsDesktop/commit/9157a485777ecc44fb60891608f39da04569db10))
* set x-gvfs-notrash mount option to prevent Nautilus trash dirs on Nextcloud ([6b10f5c](https://github.com/rgon/ncrsDesktop/commit/6b10f5cb83f5b231eca1e550289a43f61c86ed2f))
* **settings:** add GNOME GIO intermediate auto-cleanup toggle ([7fa2692](https://github.com/rgon/ncrsDesktop/commit/7fa269218da1f2c84aa6692e0cd21d165dd2e645))
* show correct size and uploading emblem for newly written files ([b65f7f0](https://github.com/rgon/ncrsDesktop/commit/b65f7f0994b49d8f7922b189174b82aae027657a))
* show storage usage in GUI for kept files, cache, and server quota ([8c8b207](https://github.com/rgon/ncrsDesktop/commit/8c8b207ede8bd8c5e13c48a9e266b73ec9074c0f))
* stale-while-revalidate for directory cache, serve cached listings immediately ([96d6c20](https://github.com/rgon/ncrsDesktop/commit/96d6c2029b19fd5985b7aa85c7e7119df616e9ca))
* support Nextcloud remote wipe to delete local data on server command ([0dd7c38](https://github.com/rgon/ncrsDesktop/commit/0dd7c386937e5c6dfa28b904aeb92a0c07a9e292))
* **sync:** keep local edits on upload failure, mark pending-sync in UI, and retry queued mutations while online ([8dcfa54](https://github.com/rgon/ncrsDesktop/commit/8dcfa549920d075ebca20071ba9a4cbca41f1637))
* **tauri:** add dismiss_error command to remove single error by timestamp ([4e71616](https://github.com/rgon/ncrsDesktop/commit/4e7161609e16ec46dda2ca4cbcfe27b0340ab9a9))
* **theme:** fetch Nextcloud server accent color from capabilities and apply to UI ([0371c1b](https://github.com/rgon/ncrsDesktop/commit/0371c1b56610140522073c8db12250be99520afc))
* thread http3 flag through notification and search clients ([f9ad7fe](https://github.com/rgon/ncrsDesktop/commit/f9ad7fe2ec9cbe8a3038f5fd544b43e68daaed62))
* **thumbnailer:** add CR3/CR2 raw thumbnail support via embedded JPEG preview ([0c39c1a](https://github.com/rgon/ncrsDesktop/commit/0c39c1ad47f2c45fa5127d2894a2184fd8129c5c))
* **thumbnailer:** add ncrs-thumbnailer for PDF with evince fallback for local files ([180dc35](https://github.com/rgon/ncrsDesktop/commit/180dc35a14f716b5ab6c47e4836a09da5a897c6a))
* **thumbnailer:** fetch NC preview via IPC instead of reading local raw file, expand to all registered RAW MIME types ([b5d44c4](https://github.com/rgon/ncrsDesktop/commit/b5d44c48cedc9394d9c4cfe22ce56a1975406de2))
* **thumbnailer:** render images via Nextcloud preview API, mount-scoped to avoid full downloads ([c85f549](https://github.com/rgon/ncrsDesktop/commit/c85f549e2e4c8bbbc892b7467c8d6e7af2f92988))
* **ui:** detect system dark/light mode and add design tokens ([0f244f5](https://github.com/rgon/ncrsDesktop/commit/0f244f569eb577130f6abd3bea6d13d66c321fc7))
* unix socket IPC server for file sync status queries ([1c2c86f](https://github.com/rgon/ncrsDesktop/commit/1c2c86fbb658409197ffa52d6143880a275b46ff))
* update nautilus extenision on runui ([b752dfa](https://github.com/rgon/ncrsDesktop/commit/b752dfad782d8ab477a18aadadebc7ed4f4b1545))
* upgrade reqwest 0.11 to 0.12 with HTTP/3 QUIC support ([ec801f1](https://github.com/rgon/ncrsDesktop/commit/ec801f12fee69d808ce6cb425b6b42336b6328d6))
* wire errors, transfers, and conflicts state into main page ([6e67051](https://github.com/rgon/ncrsDesktop/commit/6e67051d7bf94e2272be15a729b2e1d2919031ca))
* wire tauri commands to real backend state and config ([0ce8b89](https://github.com/rgon/ncrsDesktop/commit/0ce8b891679b71dce7312f75fec10c66b8c1ea43))


### Bug Fixes

* absolute window positioning wayland bypass had wrong offset ([44de636](https://github.com/rgon/ncrsDesktop/commit/44de6369a40f7a9599f7adb4df2a36a5456ca9e7))
* add done flag to stream buffer so waiters fail fast on short downloads ([e5205a0](https://github.com/rgon/ncrsDesktop/commit/e5205a029fd1153ba506e205f63e0f6abaf9508a))
* **auth:** surface 401 as error state and fix attached_poll_loop swallowing daemon errors ([5d44377](https://github.com/rgon/ncrsDesktop/commit/5d44377564fbdf54f64382549032d25c5d1c15c3))
* batch-populate IPC maps before readdir reply for immediate column data ([fc322f2](https://github.com/rgon/ncrsDesktop/commit/fc322f2a17405779ffda4973a8816d03363450d0))
* bound IPC connections to 64 with 60s read timeout to prevent thread leaks ([5917299](https://github.com/rgon/ncrsDesktop/commit/5917299fdb22c20cdebe5240b2904ba323006b04))
* **build:** sync Cargo.lock to workspace version 0.1.12 ([6ac52f7](https://github.com/rgon/ncrsDesktop/commit/6ac52f7590988baf7f4da5b18daeb1a5a0c098eb))
* **cache:** delete orphaned write_* staging files after upload completes ([af1686b](https://github.com/rgon/ncrsDesktop/commit/af1686b3b4dcc2786c8bfeff1c59d51870b6e971))
* **calendar:** manage own GOA account; never reuse manually-added entries ([7289db0](https://github.com/rgon/ncrsDesktop/commit/7289db015bf6b35c341c3f0f08eded613fc73acd))
* cancel previous download when new range read starts for same fh ([7177ee4](https://github.com/rgon/ncrsDesktop/commit/7177ee4a30af799771151e6af813b7cd390a1a7f))
* check dir cache before PROPFIND in keep-locally to avoid querying file paths as directories ([87a4e68](https://github.com/rgon/ncrsDesktop/commit/87a4e689a368096ac494ac4e53d11112cf242983))
* **ci:** add actions:read permission for artifact downloads ([45cdb14](https://github.com/rgon/ncrsDesktop/commit/45cdb14e1976f0f019cdfba2eb6d804523ad0432))
* **ci:** bump GHA actions to latest versions ([02998c9](https://github.com/rgon/ncrsDesktop/commit/02998c9e66d11b12f38e4b8b611bb93074b516a6))
* **ci:** cache release binaries; skip e2e for release-please PRs ([c8b46d6](https://github.com/rgon/ncrsDesktop/commit/c8b46d6a78e3118ab99d51b0a6b5ea37c5923594))
* **cicd:** pin pnpm version ([896c25b](https://github.com/rgon/ncrsDesktop/commit/896c25b18d07a5f9feb4a40ca2bd0ae2a704f6ad))
* **cicd:** proper release-please config ([9798a78](https://github.com/rgon/ncrsDesktop/commit/9798a78c30d8ed21183b734e12a809bfb193599a))
* **ci:** check out github.sha in build-release to guarantee binary cache hit ([8aa42a7](https://github.com/rgon/ncrsDesktop/commit/8aa42a7891f78c7c9517ebc2a17de68ef9b3d405))
* **ci:** create draft releases and publish only after successful .deb upload ([a20d247](https://github.com/rgon/ncrsDesktop/commit/a20d247ab6ef7824f6bdaf757c651b1f2da5fdc2))
* **ci:** downgrade download-artifact to v7 to match upload-artifact v7 ([84cddc8](https://github.com/rgon/ncrsDesktop/commit/84cddc8fd0a2d6c7712abb79ff2aad610c9a7642))
* **ci:** drop unused node/pnpm steps from test job ([e592f89](https://github.com/rgon/ncrsDesktop/commit/e592f8944e334a96f2c244e8d39c7260d45008a9))
* **ci:** fix sccache GHA backend and add Cargo registry cache ([6e8385a](https://github.com/rgon/ncrsDesktop/commit/6e8385a8ff792da6b3570bb5fcc2c61dcacb8a00))
* **ci:** gate release-please on e2e and run e2e on release PRs so a red suite blocks releases ([690abac](https://github.com/rgon/ncrsDesktop/commit/690abac38d4bdfd04d6dba6bcb7e606a7a4ffda1))
* **ci:** install pnpm via npm instead of pnpm/action-setup ([c270559](https://github.com/rgon/ncrsDesktop/commit/c270559a4cff74dbec5b1d7f90dbd81a5a28a737))
* **ci:** move update-lockfile guard to step level to prevent skip propagation ([d9316d2](https://github.com/rgon/ncrsDesktop/commit/d9316d22974378f29fda5966eb580a49c93f85f1))
* **ci:** pass token explicitly to release-please action ([3b2ce31](https://github.com/rgon/ncrsDesktop/commit/3b2ce3199eecf0b1627815d8672befb075ee4fd2))
* **ci:** remove global sccache rustc-wrapper; bump sccache-action to v0.0.10 ([9100bf0](https://github.com/rgon/ncrsDesktop/commit/9100bf05824d69f20801d11915e50ceef794d3e2))
* **ci:** replace upload/download-artifact with actions/cache for cross-job binary sharing ([8f6a812](https://github.com/rgon/ncrsDesktop/commit/8f6a81257c73794068811e3906bc3470d8b9c385))
* **ci:** single build job; reuse binary in e2e and release ([2d7e7bb](https://github.com/rgon/ncrsDesktop/commit/2d7e7bb722d9f08efdfbb43f031f638bd3ddaf08))
* **ci:** update debian version for CI test to run correctly, fix build and cache usage ([c0d099d](https://github.com/rgon/ncrsDesktop/commit/c0d099de1cff713dcc8d5885e37e12ac3497b805))
* clean up stale zero-byte write_* temp files on daemon startup ([cdb1599](https://github.com/rgon/ncrsDesktop/commit/cdb1599b1c2c8ad04073b8ed15731531ef73d49b))
* **core:** parallelize boot validation and update detail maps on background dir refresh ([553b5fe](https://github.com/rgon/ncrsDesktop/commit/553b5fea7f0481eed6128048fa0a6097c4f16e88))
* **core:** refuse to mount over a live mount or non-empty dir, remove mount dir on exit ([d9a6412](https://github.com/rgon/ncrsDesktop/commit/d9a6412113f1f5194b4f96d9e1a8259161d9a720))
* **core:** remove erroneous MKCOL 409 idempotent arm that silently dropped journal entries ([16d81e7](https://github.com/rgon/ncrsDesktop/commit/16d81e7438ba65c92462755becf5dae0ef5ddf02))
* **core:** surface PROPFIND auth/network errors to readdir and GUI error log ([1c06516](https://github.com/rgon/ncrsDesktop/commit/1c065164e60df6aafa3b8b857958114814255b7b))
* **core:** validate mount point before touching the IPC socket and shared state ([f196be8](https://github.com/rgon/ncrsDesktop/commit/f196be82d76580cd9f0b520f0a33d9487af817a1))
* correct Nextcloud oc:permissions flag mapping in Nautilus extension ([6685d6f](https://github.com/rgon/ncrsDesktop/commit/6685d6fa1f2745add45b7324a068a0de350333ac))
* dirty file path after PUT so Nautilus clears uploading emblem automatically ([2f39435](https://github.com/rgon/ncrsDesktop/commit/2f39435dae58178ef3a65549bca495bcba0af5e6))
* don't invalidate directories on boot if etag not changed, better atime notify-push ignore after our own propfind to prevent infinite loops ([7e5074a](https://github.com/rgon/ncrsDesktop/commit/7e5074a725ef5a1a5237d6a2885bd8dbd5444373))
* drop AutoUnmount — fuser 0.17 requires allow_other with auto_unmount ([8d2da94](https://github.com/rgon/ncrsDesktop/commit/8d2da94898881c26d4fe7cbdc0955e481ce2e511))
* enable rustls-tls for tungstenite and retry notify_push discovery on failure ([871f45e](https://github.com/rgon/ncrsDesktop/commit/871f45e898be7295540f9dab1720c76c2c9cc830))
* enforce Nextcloud oc:permissions via DefaultPermissions FUSE mount option; add perms_to_mode tests ([ced8a5c](https://github.com/rgon/ncrsDesktop/commit/ced8a5c67b9bc4b5b3a9b349847b230a7e894288))
* fetch parent directory on lookup cache miss after daemon restart ([a79b075](https://github.com/rgon/ncrsDesktop/commit/a79b0755dc50f799f6f3472fde9c5ca71f43b325))
* **fuse:** delete all GIO temps on PROPFIND, not just age-threshold ones ([921d2a7](https://github.com/rgon/ncrsDesktop/commit/921d2a77d076aede28f65c00b91fdfaf21ab86a1))
* **fuse:** evict all five maps on rmdir to match unlink cleanup ([6e8d4cb](https://github.com/rgon/ncrsDesktop/commit/6e8d4cba920a3207ef0ac3206341c698ce7b1c40))
* **fuse:** evict stale file_cache copy when a file changes on the server so reopens aren't served old bytes ([7804763](https://github.com/rgon/ncrsDesktop/commit/780476323bb7ec23d866604fb788f673140ff9a3))
* **fuse:** fail fast on a blackholed read by disabling read-client idle pooling and treating reqwest send errors as network-down ([0b12058](https://github.com/rgon/ncrsDesktop/commit/0b1205876f5cc3bfb1705dabeee01c7d01c0414e))
* **fuse:** fallback to octet-stream for MIME opens with no server content-type ([08e5673](https://github.com/rgon/ncrsDesktop/commit/08e5673e7ddf7c49f714be670db0b5cbbf190150))
* **fuse:** flip offline eagerly on network-down and add HTTP connect_timeout so an offline save falls back to cache instead of hanging ([8992a77](https://github.com/rgon/ncrsDesktop/commit/8992a773814c439dac62d1f23f96712288a3ce5b))
* **fuse:** gate read fast-paths on cache-vs-remote freshness so a server-edited file isn't served stale at the new size ([bcc3cc8](https://github.com/rgon/ncrsDesktop/commit/bcc3cc862d9f9bf8f9da22f0a9db328dfa1e63ff))
* **fuse:** guard MIME magic intercept to sz&lt;=16384 to avoid corrupting file copies ([7ec0e7a](https://github.com/rgon/ncrsDesktop/commit/7ec0e7a7e06d9fe5477c5a07cf9199f8c1ad552c))
* **fuse:** guard newly created files in uploading set to prevent ENOENT race ([9462955](https://github.com/rgon/ncrsDesktop/commit/94629552fc2c6551995b9edfca4a413cd595ed9b))
* **fuse:** make MIME-detect fallback category-aware so unknown binaries stop showing as text ([2fb74b5](https://github.com/rgon/ncrsDesktop/commit/2fb74b5de37593a986cb0a82836759c8fa9e5e4e))
* **fuse:** map image/x-dcraw to TIFF magic so camera RAW files show as images not text ([f57a0ce](https://github.com/rgon/ncrsDesktop/commit/f57a0ce98baf3259a2d4df127272eb94a6eecb0f))
* **fuse:** open MIME-detect handle O_DIRECT to stop page-cache poisoning truncating reads ([3acaf55](https://github.com/rgon/ncrsDesktop/commit/3acaf5546703d7d8fc8d03c29d945528d6e3d891))
* **fuse:** preserve in-flight uploads during concurrent dir cache PROPFIND refresh ([c41f68e](https://github.com/rgon/ncrsDesktop/commit/c41f68eae36ca25a0588f2585ce6893f084c650e))
* **fuse:** prioritize pending-PUT staging over cached copies so reads return the newest local write ([953ce66](https://github.com/rgon/ncrsDesktop/commit/953ce66f4892053d7b7d93e93d33347ae86c3a34))
* **fuse:** raise MIME magic intercept guard to 32K to cover kernel read-ahead ([8e104c9](https://github.com/rgon/ncrsDesktop/commit/8e104c9eec916661d7f97c8420ba7a7602688639))
* **fuse:** reconcile stale getattr size on read so a server-edited file isn't served truncated; pin cache freshness per open handle ([f2f76e1](https://github.com/rgon/ncrsDesktop/commit/f2f76e15469779f50e8efeab1eaf98ca9307436f))
* **fuse:** revalidate dir etag in background on every readdir so changes missed by notify-push surface without a cache purge ([757d97f](https://github.com/rgon/ncrsDesktop/commit/757d97f1fd507d8497460d69c3931a61a86367c1))
* **fuse:** serve reads of not-yet-uploaded files from pending PUT staging to avoid EIO on save-then-reopen ([f9edd74](https://github.com/rgon/ncrsDesktop/commit/f9edd74851e845580610cb57c53efc9c31c0cdcc))
* **fuse:** skip the ensure_file_cached fallback on read when offline so a blackholed server fails fast instead of retrying connect timeouts ([cce9add](https://github.com/rgon/ncrsDesktop/commit/cce9addf87eb2ae6e707f0d0ea73f7cd0d6f0f7e))
* **fuse:** update inode map on rename so saved files don't vanish ([233b5db](https://github.com/rgon/ncrsDesktop/commit/233b5dbf63c62a4faacee613d01247762ef90113))
* **fuse:** use staging file size in rename optimistic update; add flush/move logging ([5c90ded](https://github.com/rgon/ncrsDesktop/commit/5c90dedf9d227d8d7ddc658e81cae7c61ca7ce98))
* **fuse:** use TTL=0 for readdirplus entry attrs to prevent stale-size data loss ([2585b34](https://github.com/rgon/ncrsDesktop/commit/2585b34aa0bf7df4e5b426dbf8f97ccd6a0e8806))
* **fuse:** wait for in-flight PUT before issuing MOVE on rename ([dd7432f](https://github.com/rgon/ncrsDesktop/commit/dd7432f34121acdb0e1114f81194efe30708e079))
* gate child-dir PROPFIND prefetch behind aggressive_prefetch and add HTTP request throttle ([5ec0d5b](https://github.com/rgon/ncrsDesktop/commit/5ec0d5b29e682d5a0bb592f4ff2aabd17bd4b2ea))
* **goa:** write keyring credentials before accounts.conf to eliminate auth race ([70e5aa5](https://github.com/rgon/ncrsDesktop/commit/70e5aa578936b4e42f8893a799da8e0e7774cc1b))
* green-checkmark after upload; NC properties appear via forced PROPFIND ([f42c403](https://github.com/rgon/ncrsDesktop/commit/f42c4030e517ddc2541be70165bb8a72ec560063))
* guard unlink/rmdir/rename with NC D/N/V flags; block delete in create-only shared dirs ([476a2bd](https://github.com/rgon/ncrsDesktop/commit/476a2bd7bcf728e72b772dac56e8100f1a8a1f69))
* **gui:** embed tray icons at compile time and enforce a single app instance ([1afc89d](https://github.com/rgon/ncrsDesktop/commit/1afc89d6cbc519ec435f558bd1ff9b4e29c72ab8))
* **gui:** fall back to first available monitor and re-fit on scale change so the overlay covers the screen on Wayland scaled displays ([d018168](https://github.com/rgon/ncrsDesktop/commit/d018168d54cf3918bc9caefce20eb46f50cd794f))
* **gui:** pin plugin bare imports to local node_modules for production builds ([e49e969](https://github.com/rgon/ncrsDesktop/commit/e49e969dd98a28927cd41757463838be5d1965e1))
* **gui:** prevent floating panel from shrinking on HiDPI-scaled displays ([80161ed](https://github.com/rgon/ncrsDesktop/commit/80161ed571ae6cb57b4d9ad4f13768d00d44ba49))
* **gui:** regenerate app icons from the ncrs brand mark instead of the tauri template ([c87d7ef](https://github.com/rgon/ncrsDesktop/commit/c87d7efa2aefaead42fb059314a085d2613d4089))
* **gui:** reload user info and theme when window opens before daemon is ready ([759a334](https://github.com/rgon/ncrsDesktop/commit/759a3340a319dd0c1f57e10c0c9d7fb0da799a5e))
* **gui:** size overlay in logical units so fractional-scaled displays don't crop the right-anchored card ([31121af](https://github.com/rgon/ncrsDesktop/commit/31121af28fd2b097e10d099a03fa6a90f800963d))
* **gui:** use composedPath for click-outside detection of detached nodes ([74922f5](https://github.com/rgon/ncrsDesktop/commit/74922f5d2ada957fbcc80bdff1fc04fc86d19aae))
* harden bearer auth — redact secrets, stream downloads, pre_auth WS, validate creds ([eccecbe](https://github.com/rgon/ncrsDesktop/commit/eccecbe238bc19f00006d2b41eda98c1892ad768))
* harden FUSE, IPC, and Nautilus extension against panics and errors ([40aee6e](https://github.com/rgon/ncrsDesktop/commit/40aee6e3bb78320b7830da0a71872b4587389d1a))
* **http3-probe:** require HTTP/3 response version; reqwest 0.13 silently falls back to HTTP/1.1 ([a276b23](https://github.com/rgon/ncrsDesktop/commit/a276b23b2da99518e34e8af65c64208c14c73a0d))
* **http3-probe:** use async reqwest client with dedicated runtime for reliable H3 detection ([1bc9476](https://github.com/rgon/ncrsDesktop/commit/1bc9476609fff125fddfee4fe089ac3643a7a217))
* **http3:** fall back to HTTP/2 when HTTP/3 connection fails in notifications ([3b217c5](https://github.com/rgon/ncrsDesktop/commit/3b217c538d8d510ef0641db9d9cc2aa590970897))
* **http:** correct misleading http3 alt-svc comment and hint ([dfd851c](https://github.com/rgon/ncrsDesktop/commit/dfd851c8edf2558d22f954859c33d688497ed2b5))
* **http:** remove http3_prior_knowledge; use alt-svc negotiation instead ([239809c](https://github.com/rgon/ncrsDesktop/commit/239809ccbe2a97b1fd92fb6a48237ad2a62c441b))
* **ipc:** code-review fixes — children_map consistency, TOCTOU, stale-entry eviction, fallback scans ([2ae1bb8](https://github.com/rgon/ncrsDesktop/commit/2ae1bb897403eb3e3f41c11aecbe6c2caf2b260e))
* **ipc:** evict directory's own status entry on readdir refresh ([401c743](https://github.com/rgon/ncrsDesktop/commit/401c74358bebea8fd5787d0e71c0c6e74df19826))
* **ipc:** preserve concurrent lookup insertions in children_map readdir rebuild ([5c19fc5](https://github.com/rgon/ncrsDesktop/commit/5c19fc525bc8602c0ee0aa2df7414ddd37ee80b4))
* **ipc:** update detail_map and children_map atomically in readdir rebuild ([e58365e](https://github.com/rgon/ncrsDesktop/commit/e58365e3f032c7135aa3c8b10d06c38f7057a767))
* **issues:** override DaisyUI grid on alert cards, add dismiss × button ([b1a260f](https://github.com/rgon/ncrsDesktop/commit/b1a260f295e78db17a96f8e49e2fb4e275fcc9c4))
* **issues:** replace DaisyUI btn with plain icon-button classes to fix overflow ([9671b18](https://github.com/rgon/ncrsDesktop/commit/9671b182b1f3951d0544d4c891e64d2b216f0aa7))
* **keyring:** delete before save and refresh creds on 401 in notification poll ([d9ff15b](https://github.com/rgon/ncrsDesktop/commit/d9ff15bc19cc2c0849079da5233bc08cd24d219a))
* lazy-unmount stale FUSE mount before remounting on restart ([bd4cec5](https://github.com/rgon/ncrsDesktop/commit/bd4cec563fc25f0e1ffba4beb431f39039084fa4))
* **login:** correct init endpoint to /index.php/login/v2 and strip WebDAV paths from user input ([4410f91](https://github.com/rgon/ncrsDesktop/commit/4410f91c3931b208fcdf161903dd1f6fbfb4c0e2))
* **login:** send User-Agent header so Nextcloud shows app name in OAuth grant page ([e7e9320](https://github.com/rgon/ncrsDesktop/commit/e7e9320e5a59bd708e31f3eba39f835bd11b216f))
* **logout:** preserve server URL in login form after re-login flow ([e58ccaa](https://github.com/rgon/ncrsDesktop/commit/e58ccaa58ba5b789353cac037b02feb1ba372719))
* make search async with cancellation and debounce throttling ([5ac2743](https://github.com/rgon/ncrsDesktop/commit/5ac2743cf20ba0cb3e098edf43db34175366bb80))
* **mount:** remove IPC socket on FUSE teardown so remount doesn't enter attach mode ([bf5c7d5](https://github.com/rgon/ncrsDesktop/commit/bf5c7d553e5ee07fe4a10c2c4f595b37828876c7))
* **mount:** surface FUSE errors to UI and allow remount from error state ([9aa6217](https://github.com/rgon/ncrsDesktop/commit/9aa6217e0842bb4a3355effe93c56bffc9efb236))
* move DETAIL logic into update_file_info_full which Nautilus 4 actually calls ([c61c494](https://github.com/rgon/ncrsDesktop/commit/c61c49420f28251038360dccd5988657c0ad4c88))
* **nautilus:** clamp _poll_skip to 0 to prevent negative value if pool resets it mid-decrement ([38c80aa](https://github.com/rgon/ncrsDesktop/commit/38c80aad3e40e8abbafdc767380d64e8f75cf15f))
* **nautilus:** log malformed FILE_CHANGES entries; set _poll_skip before clearing _poll_running ([e0f1562](https://github.com/rgon/ncrsDesktop/commit/e0f1562890e2e4d1cca2efb46d29ecf287f57a17))
* **nautilus:** read config as utf-8 and quit nautilus in postinst ([a05fcf7](https://github.com/rgon/ncrsDesktop/commit/a05fcf705984cbe537de0dbde95e2564b820ab51))
* **nautilus:** refuse dev install when the packaged extension copy exists to avoid GObject type collision ([b3ae4f9](https://github.com/rgon/ncrsDesktop/commit/b3ae4f9a5caf76d299e16fbc4b2dc2246b4ea494))
* **net:** retry transient network errors and map timeout/network to ETIMEDOUT/EAGAIN ([bd1e882](https://github.com/rgon/ncrsDesktop/commit/bd1e8826305d7fa48876cf20b9e58f601d4bc410))
* never return empty readdir for deferred dirs, debounce dir cache saves ([bc5bc20](https://github.com/rgon/ncrsDesktop/commit/bc5bc20ca94d0351dc71d9bd940d989c43741e32))
* only prefetch subdirs/thumbnails on first readdir, deduplicate prefetch PROPFINDs ([4f5db18](https://github.com/rgon/ncrsDesktop/commit/4f5db180af9b71d6d573fa1db9dd0363d677f5fe))
* **overlay:** use maximize() so overlay respects taskbar/dock work area ([6d391d9](https://github.com/rgon/ncrsDesktop/commit/6d391d9bd580f6a52d4f7875feae22772e08937e))
* **packaging:** build GUI with custom-protocol so the deb embeds the frontend ([3f8c706](https://github.com/rgon/ncrsDesktop/commit/3f8c706a565a70d5308bcab9d8e8ba092585a601))
* parse statusCode as string to match Nextcloud Passwords API response ([25cc8bc](https://github.com/rgon/ncrsDesktop/commit/25cc8bcdef1ce3ffa5954e5af86f9eb33e3911f2))
* percent-decode paths from remotefs-webdav list_dir results ([84d13c7](https://github.com/rgon/ncrsDesktop/commit/84d13c740af5a84361daad3da230c6972a3de69d))
* percent-decode search result titles and paths ([ff294c1](https://github.com/rgon/ncrsDesktop/commit/ff294c1406a1921236fbea9473cc9b2bbdf26542))
* populate IPC maps from getattr/lookup, serve from file_cache in read, move poll off main thread ([29ae52c](https://github.com/rgon/ncrsDesktop/commit/29ae52cde2db51d2a38fb23461741d10ca15d4a4))
* prevent concurrent PROPFIND race in get_or_list_dir ([5d1a506](https://github.com/rgon/ncrsDesktop/commit/5d1a506648dcb423548c3730557920d2e3e83bfb))
* prevent recursive LOG IPC calls, increase socket timeout and recv buffer ([cd57209](https://github.com/rgon/ncrsDesktop/commit/cd57209e5f1489bb1c677f23c34ae4a19fa57b5e))
* **preview:** avoid Cow allocation in fetch_preview_bytes; preallocate PNG buffer; align probe with daemon params ([8376a3c](https://github.com/rgon/ncrsDesktop/commit/8376a3c85eafd7c1fc15fd93235ad8ce978dd241))
* **preview:** convert NC JPEG preview response to PNG for XDG thumbnail cache ([f19f259](https://github.com/rgon/ncrsDesktop/commit/f19f2599d948b45c481716207dbee010bdeada1b))
* **preview:** evict XDG fail-cache entries when thumbnail is written ([c662680](https://github.com/rgon/ncrsDesktop/commit/c662680414452afd58b19b7000786275522261c5))
* **preview:** guard thumbnail_callback with thumb_inflight to prevent duplicate NC fetches ([6c526bc](https://github.com/rgon/ncrsDesktop/commit/6c526bc7e67370f18e578e4abee19ac9cafaf7b5))
* **preview:** percent-encode file: URIs so XDG thumbnail hashes match Nautilus ([ed1a42d](https://github.com/rgon/ncrsDesktop/commit/ed1a42d796349797052ff15d28f37320fa198c14))
* **preview:** prefetch thumbnails for previewable files lacking server-cached previews ([b09cc6c](https://github.com/rgon/ncrsDesktop/commit/b09cc6c3f8ac1fdf64972f70d8f162ab1ed24193))
* **preview:** throttle on-demand RAW thumbnail fetches to avoid starving FUSE HTTP workers ([8473f2b](https://github.com/rgon/ncrsDesktop/commit/8473f2b5561834a7d88d7f690278ac8d36e345a4))
* **read:** fail copy immediately when uncached file unreachable instead of serving stale cache ([7ec0e7a](https://github.com/rgon/ncrsDesktop/commit/7ec0e7a7e06d9fe5477c5a07cf9199f8c1ad552c))
* reduce Keep Locally concurrency to 2 with yield to avoid Nautilus freeze ([708e4f5](https://github.com/rgon/ncrsDesktop/commit/708e4f547da884bc32a9da3b4eb4539371fd471c))
* reduce thumbnail batch to 4, add 200ms inter-batch and 500ms initial delay ([766fa25](https://github.com/rgon/ncrsDesktop/commit/766fa25b50a23e520ca408d521db543c84063ca5))
* reject empty bearer_token, prevent notification poll from killing other pollers ([f18f696](https://github.com/rgon/ncrsDesktop/commit/f18f6963d45d010945f3840235724315c2138253))
* **release:** annotate workspace version for release-please and unify plugin versions ([d818b7e](https://github.com/rgon/ncrsDesktop/commit/d818b7e79ada55ba356752b2f8f38ee95d8c5a66))
* **release:** use generic extra-file updater for workspace Cargo.toml ([6d0ebb2](https://github.com/rgon/ncrsDesktop/commit/6d0ebb2ee4482e357ee041b4188b523d9c662744))
* **release:** use rust release type so release-please updates workspace Cargo.toml version ([9200368](https://github.com/rgon/ncrsDesktop/commit/920036846d857197608f4755f24b6f74ad45fb43))
* **remount:** unmount active FUSE mount before restarting so mount path changes take effect ([27931d4](https://github.com/rgon/ncrsDesktop/commit/27931d4fca349877e5f8d1896daaeb4c2bf6a632))
* remove dbus reload, let inotify events handle per-file nautilus updates ([fde890d](https://github.com/rgon/ncrsDesktop/commit/fde890d2964964e41a7e9ffd5cdbfd3d6793f92c))
* remove update_file_info stub that blocked async update_file_info_full ([3d32df1](https://github.com/rgon/ncrsDesktop/commit/3d32df1cc9401e5249f26b0f27277331bf4a21ae))
* replace hard cancel with soft self-cancel, limit read throttle to 3 ([4ce0496](https://github.com/rgon/ncrsDesktop/commit/4ce049623652c19feb15f667c47bcd1318351861))
* resolve $plugins alias with absolute path and add plugin loading diagnostics ([a660c06](https://github.com/rgon/ncrsDesktop/commit/a660c06353ea6eb40f06db9246cbde9aa0cc3757))
* restore update_file_info stub, add DETAIL_ASYNC tracing ([ba29455](https://github.com/rgon/ncrsDesktop/commit/ba29455c7a5f68d25980b3d412e7801738bd927c))
* return COMPLETE synchronously for non-mount files to avoid Nautilus async overhead ([0389ab8](https://github.com/rgon/ncrsDesktop/commit/0389ab86025d64c2e282b3dbcafe1ef874a2793d))
* rewrite nautilus extension with non-blocking async update and tests ([178214c](https://github.com/rgon/ncrsDesktop/commit/178214cd812b124a86007868a3f9df086723b04c))
* **security:** chmod 0600 config file to protect app password ([5ca3337](https://github.com/rgon/ncrsDesktop/commit/5ca3337ed7df7cb2d897d770698f08fe9a5e370c))
* send self-entry immediately via channel, mark paths dirty after IPC population ([29c5a98](https://github.com/rgon/ncrsDesktop/commit/29c5a98482ae6766332ef2edf502d57303dbfdb4))
* **settings:** add gap between toggle label and checkbox ([a28ad2e](https://github.com/rgon/ncrsDesktop/commit/a28ad2eff626f0b0fa33a8c4418092f9832e233f))
* skip background download for streaming media, show blue emblem during active downloads ([94452e5](https://github.com/rgon/ncrsDesktop/commit/94452e5deb7b5143cbf19c17cecc0128f49bfd9d))
* skip IPC socket queries for files outside ncrs mount point ([eefdd6d](https://github.com/rgon/ncrsDesktop/commit/eefdd6d8d6701f3e3a2b1e025cacb61794ef156d))
* skip zero-byte cached files and clean up failed downloads ([e1f8c4a](https://github.com/rgon/ncrsDesktop/commit/e1f8c4a247a41360a101fc4ac2f2bb948c6a5ff6))
* stop eager full-file download on open, use fileId for preview API ([6c60c37](https://github.com/rgon/ncrsDesktop/commit/6c60c37e92d4a146bcf685dce5d04cbdfb7af5c3))
* suppress notify_push self-notification loop via ETag pre-check ([f5350b1](https://github.com/rgon/ncrsDesktop/commit/f5350b10b9f14a4764c6ed7c5e598b033486c94e))
* suppress self-notify kernel dentry invalidation for freshly-fetched dirs ([e492b37](https://github.com/rgon/ncrsDesktop/commit/e492b37325353ff32925a7f23a449ccc41f0bc9b))
* **sync:** drain in-flight PUT before live DELETE so lock-file create-then-delete doesn't hit 423 Locked ([53f9fd1](https://github.com/rgon/ncrsDesktop/commit/53f9fd1674141942a1ff638234325e93492c7754))
* **sync:** fsync staged bytes before journaling and make journal writes crash-durable ([9b6a533](https://github.com/rgon/ncrsDesktop/commit/9b6a533ef6228a0565da9ade1e0f1f76ba6cd4f8))
* **sync:** hold live MOVE until source PUT drains and treat rename 404 as move-source-gone conflict ([caaf4ba](https://github.com/rgon/ncrsDesktop/commit/caaf4ba4ffb6ed3b7dabfd3e3f490750fe91cade))
* **sync:** never discard local edits on server outage — treat 5xx/timeout/locked as retryable and preserve staged bytes on permanent failure ([8ee1fa1](https://github.com/rgon/ncrsDesktop/commit/8ee1fa1276b89db07b675300a3f22f364e9969f2))
* **sync:** treat live-DELETE 423/5xx as retryable so lock-file deletes don't surface 'resource locked' ([71f365e](https://github.com/rgon/ncrsDesktop/commit/71f365e0a884a7d95f0f78479fbaac4e86f885d1))
* **thumbnailer:** catch OSError from missing exiftool; use or-fallback for XDG_RUNTIME_DIR ([7c42d8d](https://github.com/rgon/ncrsDesktop/commit/7c42d8de732540d7dcc9df8cec9d9f12cf354ee9))
* **thumbnailer:** route by is_remote instead of is_local so slow/absent daemon falls through to exiftool ([881bcf3](https://github.com/rgon/ncrsDesktop/commit/881bcf3894f5d25ba67a533e23b907e05cd70d15))
* **tray:** remove Settings menu item (duplicate of Open ncRS) ([f7277d0](https://github.com/rgon/ncrsDesktop/commit/f7277d066e7d51ed8abd9476861286c5749014fa))
* **typecheck:** resolve all svelte-check errors ([e80c94c](https://github.com/rgon/ncrsDesktop/commit/e80c94cccc630d09167db14b192c51c2cf20f485))
* **ui:** expandable error cards, pointerdown close, ENOSPC retry storm ([9267a5f](https://github.com/rgon/ncrsDesktop/commit/9267a5f4daf3535f1b2b0802e0e45321c53c116c))
* **ui:** fix server label clipping and avatar dropdown overflow ([43cd4c5](https://github.com/rgon/ncrsDesktop/commit/43cd4c5f8555da7093b8e64c7bb0eae780503550))
* **ui:** move [@const](https://github.com/const) tags to be direct children of {#if} block ([4efb8ec](https://github.com/rgon/ncrsDesktop/commit/4efb8ecbff64f133c3f64d6a2465d3794d523123))
* **ui:** register event listeners before loading initial state to avoid notification race ([8219529](https://github.com/rgon/ncrsDesktop/commit/821952947de27a10a7c0ede88c6210cd9f4da642))
* **ui:** show 'Log in' button for auth errors instead of 'Remount' ([3144987](https://github.com/rgon/ncrsDesktop/commit/31449872c6e1295a161e284447713eae0b746f77))
* **ui:** show FUSE error message in sync label instead of invisible alert span ([bd973bc](https://github.com/rgon/ncrsDesktop/commit/bd973bc2ee68afbe717b9c62fea4fb7116ffd455))
* unmount FUSE on quit, handle existing mount point, and sync tray state on all transitions ([4a853e3](https://github.com/rgon/ncrsDesktop/commit/4a853e3109d4c6ab111546b11ee1d28d981472e4))
* update mount_ncfs call signature and harden MKCOL/DELETE ops ([d745ae9](https://github.com/rgon/ncrsDesktop/commit/d745ae9b2e9de00cc9dc2e16776d02f4b928c839))
* use download arrow emblem for partial-download folders ([b93a90d](https://github.com/rgon/ncrsDesktop/commit/b93a90d5ff67bdd0fbc870b2978ee3b2033558a1))
* use Nautilus.FileInfo.lookup for invalidation instead of storing stale GObject refs ([85b4aef](https://github.com/rgon/ncrsDesktop/commit/85b4aef908436acf8629342f6fdad067dedbec1f))
* use shared buffer with condvar for incremental read-ahead streaming ([1ac65bc](https://github.com/rgon/ncrsDesktop/commit/1ac65bccec0f3d020083f36f0899a98cec44096e))
* use sync update_file_info with background cache for non-blocking NC columns ([d2efdd3](https://github.com/rgon/ncrsDesktop/commit/d2efdd3ef4ad2b208d3613b037c870365c42fa24))
* use synchronous DETAIL IPC in update_file_info for immediate NC columns ([12860a0](https://github.com/rgon/ncrsDesktop/commit/12860a04086ffc9aa53ee397410e96ff0095506a))
* wait for PROPFIND completion instead of 2s timeout, increase IPC socket timeout ([96b4fa7](https://github.com/rgon/ncrsDesktop/commit/96b4fa7f245e8631fed8bde6910628e2ec9333d7))
* **webdav:** use Overwrite: T in MOVE so rename atomically replaces existing destinations: WebDAV move had different behaviour to POSIX move. ([d7ccaa3](https://github.com/rgon/ncrsDesktop/commit/d7ccaa3459ae526614d0c9e29f5b08b0ef46e731))
* wrap all Nautilus extension callbacks in try/except to prevent crashes ([fa678b8](https://github.com/rgon/ncrsDesktop/commit/fa678b8fa2af962322ec9778cc84ca1a95bd6ea5))


### Performance Improvements

* add prefetch_throttle so aggressive_prefetch doesn't compete with READDIR ([213a34d](https://github.com/rgon/ncrsDesktop/commit/213a34dffdb9c9616e2e38d52ba890f704ec3cb2))
* add throughput metrics to range read logging ([82bb0dd](https://github.com/rgon/ncrsDesktop/commit/82bb0dd7ebf43425eb4613b129c31acb40b68fcc))
* **build:** merge two cargo build passes into one to avoid recompiling shared deps ([51b93b2](https://github.com/rgon/ncrsDesktop/commit/51b93b2ac62b536f5fe860e19694415571e0a1df))
* cap thumbnail and subdirectory prefetching for directories &gt;200 entries ([01f2a33](https://github.com/rgon/ncrsDesktop/commit/01f2a33c9a3e6964a49cb10a75ee4e9a89fa264b))
* chain prefetch one level deeper to eliminate pause between traversal waves ([85061be](https://github.com/rgon/ncrsDesktop/commit/85061be28a5ff20279f4a633bcc46f29a119540e))
* **ci:** unit test against release build to avoid re-building both versions in CI ([ab82031](https://github.com/rgon/ncrsDesktop/commit/ab82031bda898e0c2da3af83397d97cd83af1496))
* fix condvar wake + suppress proactive_refresh during traversal ([b153765](https://github.com/rgon/ncrsDesktop/commit/b1537658e0460a7bc2c8ea585b023ac3e3f073d0))
* **fuse:** deduplicate concurrent thumbnail-prefetch threads per directory ([861178d](https://github.com/rgon/ncrsDesktop/commit/861178d061c8e324f42488e0bca599a80f400bf6))
* **fuse:** defer O(n) map retain() calls to after reply.ok() in readdir ([a85c2dd](https://github.com/rgon/ncrsDesktop/commit/a85c2dd7bd4c134c429d629e4283ec75c83d499a))
* **fuse:** intercept GLib MIME detection opens with synthetic magic bytes ([08e5673](https://github.com/rgon/ncrsDesktop/commit/08e5673e7ddf7c49f714be670db0b5cbbf190150))
* **fuse:** prefetch small files on open() to parallelise MIME magic-byte detection ([9cc6505](https://github.com/rgon/ncrsDesktop/commit/9cc65059f614f0b03847445a25676bf44a923bd4))
* **fuse:** raise attr TTL to 30 s to avoid per-second kernel re-queries ([8fc19cf](https://github.com/rgon/ncrsDesktop/commit/8fc19cfc0ccf3057b4b8e8cb7b41a92a8e417293))
* **fuse:** release cache lock before reply.add() loop to unblock concurrent getattr ([fdb907a](https://github.com/rgon/ncrsDesktop/commit/fdb907aa2ff00dce64b3eeac39ddc7edeb683c70))
* **fuse:** release cache lock before scanning dir entries in getattr and lookup ([52cf175](https://github.com/rgon/ncrsDesktop/commit/52cf175b68e7308c01224ca64077d5eefe442e44))
* **fuse:** short-circuit second metadata() stat when kept_path already matches ([3769185](https://github.com/rgon/ncrsDesktop/commit/37691851b3affb90558a3e2a4e10422d50980f18))
* **gui:** apply pushed snapshots per-field so only changed fields re-parse and re-emit ([3ad2cef](https://github.com/rgon/ncrsDesktop/commit/3ad2cef17fcda06af861a60cc55dc8cf15468760))
* **gui:** end idle tray CPU by subscribing to daemon pushes and freeing the webview on close ([4fe90ab](https://github.com/rgon/ncrsDesktop/commit/4fe90abcc4e64f6cff1c171c235f647e9ae843f8))
* **gui:** use jemalloc to bound RSS from read-ahead buffer churn ([89e02e6](https://github.com/rgon/ncrsDesktop/commit/89e02e6aeeac95ef4ab2411cb81216e2d1e1930c))
* increase read-ahead to 64MB and add background prefetching ([7869377](https://github.com/rgon/ncrsDesktop/commit/7869377f63da96ec83426278416761a48c757ef5))
* increase thumb prefetch concurrency (batch 32, cap 200) and fix stray lock unwrap ([f58528e](https://github.com/rgon/ncrsDesktop/commit/f58528e5939f0f56102837a695496dfc75e7669a))
* **ipc:** add ChildrenMap index for O(dir_size) detail/status lookups ([2241bf5](https://github.com/rgon/ncrsDesktop/commit/2241bf5488b6ea84b1bbab27de8ad55440518592))
* **ipc:** aggregate DETAILDIR directory statuses in one pass instead of O(subdirs*N) scans ([0b7fe71](https://github.com/rgon/ncrsDesktop/commit/0b7fe71f5ea3f6075a04d5bd1c33fb81342e1bce))
* **ipc:** cache the daemon state snapshot's journal JSON by version and skip rebuilding unchanged ticks ([e2c994e](https://github.com/rgon/ncrsDesktop/commit/e2c994ed044d1d383bb71ed83c52487c79ca58a9))
* **ipc:** convert StatusMap/FileDetailMap/SharedSet/FileIdMap to RwLock to fix DETAILDIR starvation ([da95012](https://github.com/rgon/ncrsDesktop/commit/da950122f6cded587c9809882a9beda0ba424535))
* **ipc:** demote DETAIL log to debug, fix thumbnailer crash on malformed JPEG ([a0c247a](https://github.com/rgon/ncrsDesktop/commit/a0c247aad1ba26fc008c4b26f06ac750e32af86b))
* **ipc:** drop status/detail locks before joining DETAILDIR reply so huge dirs don't stall FUSE ([7632fae](https://github.com/rgon/ncrsDesktop/commit/7632fae39a72a552de5278c3b21da6a8c4ece92a))
* **ipc:** pre-populate detail/shared/fileid maps before readdir reply.ok() to fix race with Nautilus extension DETAIL queries ([d246cf8](https://github.com/rgon/ncrsDesktop/commit/d246cf85c9b6be8b136dc478e4bebfaa0d6cd1fa))
* make lookup/getattr cache-only, no PROPFIND triggered by Nautilus file scanning ([9ac60cf](https://github.com/rgon/ncrsDesktop/commit/9ac60cf1761eb931a575fc9ca1ff574457f01bf1))
* **nautilus:** avoid FUSE utimes upcall for M-type FILE_CHANGES events ([396829b](https://github.com/rgon/ncrsDesktop/commit/396829b0249342a85a27e2cec43dcf44057fbf82))
* **nautilus:** bound the directory metadata cache and precompute the mount prefix ([d44e126](https://github.com/rgon/ncrsDesktop/commit/d44e1266619e3219139a22f277690a0f3046ffa2))
* **nautilus:** cap pending invalidations and split pdf thumbnail throttle ([64f2e98](https://github.com/rgon/ncrsDesktop/commit/64f2e98bd7d87670c018a4f11171c2becc7c787f))
* **nautilus:** guard poll loop against backpressure when daemon is slow ([30c32c0](https://github.com/rgon/ncrsDesktop/commit/30c32c044bfbff679bfd87f6f142d8e53ed7bcc5))
* **nautilus:** make update_file_info async via update_file_info_full + IN_PROGRESS ([bd1eaa8](https://github.com/rgon/ncrsDesktop/commit/bd1eaa82771e431758a4c4665b96b69b8ae528ca))
* **nautilus:** patch changed cache entries in place instead of refetching the whole directory on notify_push updates ([049e167](https://github.com/rgon/ncrsDesktop/commit/049e167b3fb69e0d175165bd38af8793749c31f1))
* **nautilus:** remove per-file STATUS queries from get_file_items GTK thread ([2dcc8fb](https://github.com/rgon/ncrsDesktop/commit/2dcc8fb21869a6411c5f129d4e17544fae590a02))
* **nautilus:** repaint only requested children after a fetch to avoid O(dir) invalidation on the main thread ([608625c](https://github.com/rgon/ncrsDesktop/commit/608625c9cce20321a1250d256ef075023045375f))
* **nautilus:** replace blocking _poll_keep_done loop with GLib timeout ([4a937c0](https://github.com/rgon/ncrsDesktop/commit/4a937c0821b9182e3981bfe38c438f56695ffa9d))
* **nautilus:** resolve path via single get_uri() and read cache lock-free in the per-file hot path ([d181217](https://github.com/rgon/ncrsDesktop/commit/d181217b5f047f7a9a0bf90ffc05e4fa7595fa1b))
* **nautilus:** serve file info synchronously from a per-directory cache to fix slow listings and handle=(nil) spam ([e11853d](https://github.com/rgon/ncrsDesktop/commit/e11853d8f00c5d138eed1561a874dcef424d903c))
* **nautilus:** skip empty file attributes and memoize perms to cut per-file GObject calls ([f389cb4](https://github.com/rgon/ncrsDesktop/commit/f389cb41cda0b57b5521ae71b161d12c20a2fac2))
* **nautilus:** warm dir cache synchronously on cold open to kill O(N) repaint storm ([eaf3414](https://github.com/rgon/ncrsDesktop/commit/eaf34144b43a2022d92296274e23aafdebf31473))
* non-blocking FUSE handlers with per-call WebDAV timeout ([4df097d](https://github.com/rgon/ncrsDesktop/commit/4df097da1a93c25105ccd63b6d3a187bfe28f06b))
* populate IPC maps only once per directory and cap CHANGES to 500 paths ([19147fd](https://github.com/rgon/ncrsDesktop/commit/19147fda83cf627c5e73701327dd2a2e5a64b908))
* prefetch child dirs concurrently on readdir using pending_dirs ([648b591](https://github.com/rgon/ncrsDesktop/commit/648b591720aebf2eb32a5d41963ede2411f0bdcc))
* **preview:** increase thumbnail throughput; add fetch/convert timing logs ([738fe16](https://github.com/rgon/ncrsDesktop/commit/738fe167957deac999fc3441cd961b98b73064d0))
* **preview:** write thumbnail via tmp+rename to prevent concurrent corruption ([8e5227d](https://github.com/rgon/ncrsDesktop/commit/8e5227d7784743e8e029299f232670054268462a))
* reduce prefetch contention and thumbnail size for faster directory loads ([2034acc](https://github.com/rgon/ncrsDesktop/commit/2034acc0be6de714e9242191b3a2021a4e745e40))
* reduce thumbnail batch size and gate on active streams ([f8c10e2](https://github.com/rgon/ncrsDesktop/commit/f8c10e27a704a342068771a1589dfea183aa1e24))
* separate read throttle, increase read pool to 8, enable tcp_nodelay ([d430848](https://github.com/rgon/ncrsDesktop/commit/d43084829dfeb3b21f73cfc3cf9589938e1b95db))
* share HTTP client across requests and parallelize thumbnail prefetch ([e8d1fb0](https://github.com/rgon/ncrsDesktop/commit/e8d1fb0865119b5649d9a5e5303714a9c212ba90))
* short-circuit readdir continuation pages from cache ([78395d7](https://github.com/rgon/ncrsDesktop/commit/78395d783f504395f60881fb1e98b5d105f4c5eb))
* skip read throttle for small reads without read-ahead ([b9e2afb](https://github.com/rgon/ncrsDesktop/commit/b9e2afbf43e4e50e8a0c091643cec5e9cd667a80))
* split IPC population into per-map short locks to reduce FUSE contention ([62e00b4](https://github.com/rgon/ncrsDesktop/commit/62e00b401fa318b1723f4cb46d84f532b96532f2))
* stop flooding dirty set on notify_push file change events ([5b50437](https://github.com/rgon/ncrsDesktop/commit/5b504370b144935b8438e42b5adbade2bbfa1eb6))
* stream range reads to reply with first bytes immediately ([f57c34c](https://github.com/rgon/ncrsDesktop/commit/f57c34c256c2051a928e37e25b10da0295f12f39))
* stream XML parsing directly from HTTP response instead of buffering ([c79644c](https://github.com/rgon/ncrsDesktop/commit/c79644cd2b4986832404e73268f0197709c95fa7))
* switch back to async update_file_info_full with 32-worker pool for parallel DETAIL queries ([b988fec](https://github.com/rgon/ncrsDesktop/commit/b988fece1b1300dd606d53036c465c655b167ba3))
* switch Nautilus DETAIL queries to synchronous IPC, eliminate thread pool bottleneck ([f1f8691](https://github.com/rgon/ncrsDesktop/commit/f1f86912217ba142e149ba76c84878aa7ac80f5d))
* **thumbnailer,extension:** fix hang risks, spurious NC ops, and idle poll overhead ([836cae3](https://github.com/rgon/ncrsDesktop/commit/836cae308e7d781ff41240d37ae1ec3b002bbe1b))
* **upload:** stream PUT from staging file instead of loading into RAM ([07dc81a](https://github.com/rgon/ncrsDesktop/commit/07dc81a47b8ec5b4369fd0610e6e017e9727362d))
* use Arc&lt;Vec&lt;DavEntry&gt;&gt; in dir cache to eliminate O(N) clones per FUSE call ([9796f99](https://github.com/rgon/ncrsDesktop/commit/9796f99f495909c475ae3bed99289ea29c4b10ec))
* WebDAV connection pool and speculative subdirectory prefetching ([adadcc6](https://github.com/rgon/ncrsDesktop/commit/adadcc68e06eae2ab55219feb818d252b142c050))

## [0.1.48](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.47...ncrs-v0.1.48) (2026-07-28)


### Performance Improvements

* **gui:** use jemalloc to bound RSS from read-ahead buffer churn ([89e02e6](https://github.com/rgon/ncrsDesktop/commit/89e02e6aeeac95ef4ab2411cb81216e2d1e1930c))

## [0.1.47](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.46...ncrs-v0.1.47) (2026-07-26)


### Bug Fixes

* **ci:** check out github.sha in build-release to guarantee binary cache hit ([8aa42a7](https://github.com/rgon/ncrsDesktop/commit/8aa42a7891f78c7c9517ebc2a17de68ef9b3d405))
* **ci:** create draft releases and publish only after successful .deb upload ([a20d247](https://github.com/rgon/ncrsDesktop/commit/a20d247ab6ef7824f6bdaf757c651b1f2da5fdc2))

## [0.1.46](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.45...ncrs-v0.1.46) (2026-07-26)


### Features

* **packaging:** add AppStream metainfo with OARS rating and hardware hints ([880918b](https://github.com/rgon/ncrsDesktop/commit/880918bb77e1e5c5ba7975b106275525f6c4c2aa))


### Bug Fixes

* **ci:** move update-lockfile guard to step level to prevent skip propagation ([d9316d2](https://github.com/rgon/ncrsDesktop/commit/d9316d22974378f29fda5966eb580a49c93f85f1))
* **overlay:** use maximize() so overlay respects taskbar/dock work area ([6d391d9](https://github.com/rgon/ncrsDesktop/commit/6d391d9bd580f6a52d4f7875feae22772e08937e))

## [0.1.45](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.44...ncrs-v0.1.45) (2026-07-26)


### Bug Fixes

* absolute window positioning wayland bypass had wrong offset ([44de636](https://github.com/rgon/ncrsDesktop/commit/44de6369a40f7a9599f7adb4df2a36a5456ca9e7))
* **gui:** reload user info and theme when window opens before daemon is ready ([759a334](https://github.com/rgon/ncrsDesktop/commit/759a3340a319dd0c1f57e10c0c9d7fb0da799a5e))

## [0.1.44](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.43...ncrs-v0.1.44) (2026-07-26)


### Bug Fixes

* **gui:** fall back to first available monitor and re-fit on scale change so the overlay covers the screen on Wayland scaled displays ([d018168](https://github.com/rgon/ncrsDesktop/commit/d018168d54cf3918bc9caefce20eb46f50cd794f))

## [0.1.43](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.42...ncrs-v0.1.43) (2026-07-25)


### Bug Fixes

* **fuse:** revalidate dir etag in background on every readdir so changes missed by notify-push surface without a cache purge ([757d97f](https://github.com/rgon/ncrsDesktop/commit/757d97f1fd507d8497460d69c3931a61a86367c1))

## [0.1.42](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.41...ncrs-v0.1.42) (2026-07-24)


### Bug Fixes

* **fuse:** make MIME-detect fallback category-aware so unknown binaries stop showing as text ([2fb74b5](https://github.com/rgon/ncrsDesktop/commit/2fb74b5de37593a986cb0a82836759c8fa9e5e4e))
* **fuse:** map image/x-dcraw to TIFF magic so camera RAW files show as images not text ([f57a0ce](https://github.com/rgon/ncrsDesktop/commit/f57a0ce98baf3259a2d4df127272eb94a6eecb0f))

## [0.1.41](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.40...ncrs-v0.1.41) (2026-07-22)


### Features

* **ipc:** push daemon state to subscribers via a SUBSCRIBE verb so the GUI stops polling ([28efecd](https://github.com/rgon/ncrsDesktop/commit/28efecdc911ecb58ab5c72f06d01364a196a355f))


### Performance Improvements

* **gui:** apply pushed snapshots per-field so only changed fields re-parse and re-emit ([3ad2cef](https://github.com/rgon/ncrsDesktop/commit/3ad2cef17fcda06af861a60cc55dc8cf15468760))
* **gui:** end idle tray CPU by subscribing to daemon pushes and freeing the webview on close ([4fe90ab](https://github.com/rgon/ncrsDesktop/commit/4fe90abcc4e64f6cff1c171c235f647e9ae843f8))
* **ipc:** cache the daemon state snapshot's journal JSON by version and skip rebuilding unchanged ticks ([e2c994e](https://github.com/rgon/ncrsDesktop/commit/e2c994ed044d1d383bb71ed83c52487c79ca58a9))

## [0.1.40](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.39...ncrs-v0.1.40) (2026-07-18)


### Bug Fixes

* **fuse:** fail fast on a blackholed read by disabling read-client idle pooling and treating reqwest send errors as network-down ([0b12058](https://github.com/rgon/ncrsDesktop/commit/0b1205876f5cc3bfb1705dabeee01c7d01c0414e))
* **fuse:** flip offline eagerly on network-down and add HTTP connect_timeout so an offline save falls back to cache instead of hanging ([8992a77](https://github.com/rgon/ncrsDesktop/commit/8992a773814c439dac62d1f23f96712288a3ce5b))
* **fuse:** skip the ensure_file_cached fallback on read when offline so a blackholed server fails fast instead of retrying connect timeouts ([cce9add](https://github.com/rgon/ncrsDesktop/commit/cce9addf87eb2ae6e707f0d0ea73f7cd0d6f0f7e))

## [0.1.39](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.38...ncrs-v0.1.39) (2026-07-18)


### Features

* **gui:** add purge local cache action that re-downloads fresh copies while preserving unsynced edits ([c7225d3](https://github.com/rgon/ncrsDesktop/commit/c7225d3736e70b30445dd85b717a92bb59fe38c0))


### Bug Fixes

* **fuse:** evict stale file_cache copy when a file changes on the server so reopens aren't served old bytes ([7804763](https://github.com/rgon/ncrsDesktop/commit/780476323bb7ec23d866604fb788f673140ff9a3))
* **fuse:** gate read fast-paths on cache-vs-remote freshness so a server-edited file isn't served stale at the new size ([bcc3cc8](https://github.com/rgon/ncrsDesktop/commit/bcc3cc862d9f9bf8f9da22f0a9db328dfa1e63ff))
* **fuse:** reconcile stale getattr size on read so a server-edited file isn't served truncated; pin cache freshness per open handle ([f2f76e1](https://github.com/rgon/ncrsDesktop/commit/f2f76e15469779f50e8efeab1eaf98ca9307436f))

## [0.1.38](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.37...ncrs-v0.1.38) (2026-07-18)


### Bug Fixes

* **sync:** drain in-flight PUT before live DELETE so lock-file create-then-delete doesn't hit 423 Locked ([53f9fd1](https://github.com/rgon/ncrsDesktop/commit/53f9fd1674141942a1ff638234325e93492c7754))
* **sync:** treat live-DELETE 423/5xx as retryable so lock-file deletes don't surface 'resource locked' ([71f365e](https://github.com/rgon/ncrsDesktop/commit/71f365e0a884a7d95f0f78479fbaac4e86f885d1))

## [0.1.37](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.36...ncrs-v0.1.37) (2026-07-17)


### Features

* **sync:** keep local edits on upload failure, mark pending-sync in UI, and retry queued mutations while online ([8dcfa54](https://github.com/rgon/ncrsDesktop/commit/8dcfa549920d075ebca20071ba9a4cbca41f1637))


### Bug Fixes

* **fuse:** prioritize pending-PUT staging over cached copies so reads return the newest local write ([953ce66](https://github.com/rgon/ncrsDesktop/commit/953ce66f4892053d7b7d93e93d33347ae86c3a34))
* **fuse:** serve reads of not-yet-uploaded files from pending PUT staging to avoid EIO on save-then-reopen ([f9edd74](https://github.com/rgon/ncrsDesktop/commit/f9edd74851e845580610cb57c53efc9c31c0cdcc))
* **sync:** fsync staged bytes before journaling and make journal writes crash-durable ([9b6a533](https://github.com/rgon/ncrsDesktop/commit/9b6a533ef6228a0565da9ade1e0f1f76ba6cd4f8))
* **sync:** hold live MOVE until source PUT drains and treat rename 404 as move-source-gone conflict ([caaf4ba](https://github.com/rgon/ncrsDesktop/commit/caaf4ba4ffb6ed3b7dabfd3e3f490750fe91cade))
* **sync:** never discard local edits on server outage — treat 5xx/timeout/locked as retryable and preserve staged bytes on permanent failure ([8ee1fa1](https://github.com/rgon/ncrsDesktop/commit/8ee1fa1276b89db07b675300a3f22f364e9969f2))

## [0.1.36](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.35...ncrs-v0.1.36) (2026-07-17)


### Bug Fixes

* **gui:** size overlay in logical units so fractional-scaled displays don't crop the right-anchored card ([31121af](https://github.com/rgon/ncrsDesktop/commit/31121af28fd2b097e10d099a03fa6a90f800963d))

## [0.1.35](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.34...ncrs-v0.1.35) (2026-07-17)


### Bug Fixes

* **ci:** gate release-please on e2e and run e2e on release PRs so a red suite blocks releases ([690abac](https://github.com/rgon/ncrsDesktop/commit/690abac38d4bdfd04d6dba6bcb7e606a7a4ffda1))
* **fuse:** open MIME-detect handle O_DIRECT to stop page-cache poisoning truncating reads ([3acaf55](https://github.com/rgon/ncrsDesktop/commit/3acaf5546703d7d8fc8d03c29d945528d6e3d891))

## [0.1.34](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.33...ncrs-v0.1.34) (2026-07-16)


### Bug Fixes

* **fuse:** evict all five maps on rmdir to match unlink cleanup ([6e8d4cb](https://github.com/rgon/ncrsDesktop/commit/6e8d4cba920a3207ef0ac3206341c698ce7b1c40))

## [0.1.33](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.32...ncrs-v0.1.33) (2026-07-16)


### Bug Fixes

* **fuse:** use TTL=0 for readdirplus entry attrs to prevent stale-size data loss ([2585b34](https://github.com/rgon/ncrsDesktop/commit/2585b34aa0bf7df4e5b426dbf8f97ccd6a0e8806))

## [0.1.32](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.31...ncrs-v0.1.32) (2026-07-16)


### Features

* **fuse:** implement readdirplus to bundle entry attributes and cut per-file getattr ([1ef19a9](https://github.com/rgon/ncrsDesktop/commit/1ef19a9b166637854471cf3140f890fc799fd464))
* **thumbnailer:** render images via Nextcloud preview API, mount-scoped to avoid full downloads ([c85f549](https://github.com/rgon/ncrsDesktop/commit/c85f549e2e4c8bbbc892b7467c8d6e7af2f92988))


### Bug Fixes

* **fuse:** raise MIME magic intercept guard to 32K to cover kernel read-ahead ([8e104c9](https://github.com/rgon/ncrsDesktop/commit/8e104c9eec916661d7f97c8420ba7a7602688639))


### Performance Improvements

* **nautilus:** resolve path via single get_uri() and read cache lock-free in the per-file hot path ([d181217](https://github.com/rgon/ncrsDesktop/commit/d181217b5f047f7a9a0bf90ffc05e4fa7595fa1b))
* **nautilus:** skip empty file attributes and memoize perms to cut per-file GObject calls ([f389cb4](https://github.com/rgon/ncrsDesktop/commit/f389cb41cda0b57b5521ae71b161d12c20a2fac2))
* **nautilus:** warm dir cache synchronously on cold open to kill O(N) repaint storm ([eaf3414](https://github.com/rgon/ncrsDesktop/commit/eaf34144b43a2022d92296274e23aafdebf31473))

## [0.1.31](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.30...ncrs-v0.1.31) (2026-07-15)


### Bug Fixes

* **ipc:** code-review fixes — children_map consistency, TOCTOU, stale-entry eviction, fallback scans ([2ae1bb8](https://github.com/rgon/ncrsDesktop/commit/2ae1bb897403eb3e3f41c11aecbe6c2caf2b260e))
* **ipc:** evict directory's own status entry on readdir refresh ([401c743](https://github.com/rgon/ncrsDesktop/commit/401c74358bebea8fd5787d0e71c0c6e74df19826))
* **ipc:** preserve concurrent lookup insertions in children_map readdir rebuild ([5c19fc5](https://github.com/rgon/ncrsDesktop/commit/5c19fc525bc8602c0ee0aa2df7414ddd37ee80b4))
* **ipc:** update detail_map and children_map atomically in readdir rebuild ([e58365e](https://github.com/rgon/ncrsDesktop/commit/e58365e3f032c7135aa3c8b10d06c38f7057a767))
* **nautilus:** read config as utf-8 and quit nautilus in postinst ([a05fcf7](https://github.com/rgon/ncrsDesktop/commit/a05fcf705984cbe537de0dbde95e2564b820ab51))


### Performance Improvements

* **ci:** unit test against release build to avoid re-building both versions in CI ([ab82031](https://github.com/rgon/ncrsDesktop/commit/ab82031bda898e0c2da3af83397d97cd83af1496))
* **ipc:** add ChildrenMap index for O(dir_size) detail/status lookups ([2241bf5](https://github.com/rgon/ncrsDesktop/commit/2241bf5488b6ea84b1bbab27de8ad55440518592))
* **ipc:** convert StatusMap/FileDetailMap/SharedSet/FileIdMap to RwLock to fix DETAILDIR starvation ([da95012](https://github.com/rgon/ncrsDesktop/commit/da950122f6cded587c9809882a9beda0ba424535))
* **nautilus:** cap pending invalidations and split pdf thumbnail throttle ([64f2e98](https://github.com/rgon/ncrsDesktop/commit/64f2e98bd7d87670c018a4f11171c2becc7c787f))

## [0.1.30](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.29...ncrs-v0.1.30) (2026-07-15)


### Bug Fixes

* **ci:** update debian version for CI test to run correctly, fix build and cache usage ([c0d099d](https://github.com/rgon/ncrsDesktop/commit/c0d099de1cff713dcc8d5885e37e12ac3497b805))

## [0.1.29](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.28...ncrs-v0.1.29) (2026-07-15)


### Bug Fixes

* **ci:** replace upload/download-artifact with actions/cache for cross-job binary sharing ([8f6a812](https://github.com/rgon/ncrsDesktop/commit/8f6a81257c73794068811e3906bc3470d8b9c385))

## [0.1.28](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.27...ncrs-v0.1.28) (2026-07-15)


### Bug Fixes

* **ci:** add actions:read permission for artifact downloads ([45cdb14](https://github.com/rgon/ncrsDesktop/commit/45cdb14e1976f0f019cdfba2eb6d804523ad0432))
* **ci:** downgrade download-artifact to v7 to match upload-artifact v7 ([84cddc8](https://github.com/rgon/ncrsDesktop/commit/84cddc8fd0a2d6c7712abb79ff2aad610c9a7642))
* **ci:** fix sccache GHA backend and add Cargo registry cache ([6e8385a](https://github.com/rgon/ncrsDesktop/commit/6e8385a8ff792da6b3570bb5fcc2c61dcacb8a00))

## [0.1.27](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.26...ncrs-v0.1.27) (2026-07-15)


### Bug Fixes

* **ci:** bump GHA actions to latest versions ([02998c9](https://github.com/rgon/ncrsDesktop/commit/02998c9e66d11b12f38e4b8b611bb93074b516a6))
* **ci:** cache release binaries; skip e2e for release-please PRs ([c8b46d6](https://github.com/rgon/ncrsDesktop/commit/c8b46d6a78e3118ab99d51b0a6b5ea37c5923594))

## [0.1.26](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.25...ncrs-v0.1.26) (2026-07-15)


### Bug Fixes

* **ci:** single build job; reuse binary in e2e and release ([2d7e7bb](https://github.com/rgon/ncrsDesktop/commit/2d7e7bb722d9f08efdfbb43f031f638bd3ddaf08))

## [0.1.25](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.24...ncrs-v0.1.25) (2026-07-14)


### Bug Fixes

* **ci:** remove global sccache rustc-wrapper; bump sccache-action to v0.0.10 ([9100bf0](https://github.com/rgon/ncrsDesktop/commit/9100bf05824d69f20801d11915e50ceef794d3e2))

## [0.1.24](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.23...ncrs-v0.1.24) (2026-07-14)


### Features

* **cli:** add --url/--username/--password overrides and remove http3 probe ([a24a1f0](https://github.com/rgon/ncrsDesktop/commit/a24a1f0e8c02a669cc322ba9cbfb4cb8f99e4e17))


### Bug Fixes

* **fuse:** use staging file size in rename optimistic update; add flush/move logging ([5c90ded](https://github.com/rgon/ncrsDesktop/commit/5c90dedf9d227d8d7ddc658e81cae7c61ca7ce98))
* **fuse:** wait for in-flight PUT before issuing MOVE on rename ([dd7432f](https://github.com/rgon/ncrsDesktop/commit/dd7432f34121acdb0e1114f81194efe30708e079))
* **ui:** register event listeners before loading initial state to avoid notification race ([8219529](https://github.com/rgon/ncrsDesktop/commit/821952947de27a10a7c0ede88c6210cd9f4da642))

## [0.1.23](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.22...ncrs-v0.1.23) (2026-07-14)


### Features

* **config:** auto-normalize DAV URL from any user-supplied format ([1f582b6](https://github.com/rgon/ncrsDesktop/commit/1f582b6ba38341d6b439ac5f7a50852754409db0))
* **hpb:** add degraded state and orange tray icon for notify_push failures ([0cfa568](https://github.com/rgon/ncrsDesktop/commit/0cfa568196cc4aedf7c725cdb19974e4b0c5896c))
* **logging:** replace env_logger with tauri-plugin-log to surface warnings in DevTools ([fd341e9](https://github.com/rgon/ncrsDesktop/commit/fd341e90918222f406362113fd7422f57bfe2198))
* **logging:** wire @tauri-apps/plugin-log JS package and attachConsole for DevTools output ([9eff244](https://github.com/rgon/ncrsDesktop/commit/9eff2442d0bbc62aeabcaec7f70908b5e1df9328))


### Bug Fixes

* **auth:** surface 401 as error state and fix attached_poll_loop swallowing daemon errors ([5d44377](https://github.com/rgon/ncrsDesktop/commit/5d44377564fbdf54f64382549032d25c5d1c15c3))
* **fuse:** fallback to octet-stream for MIME opens with no server content-type ([08e5673](https://github.com/rgon/ncrsDesktop/commit/08e5673e7ddf7c49f714be670db0b5cbbf190150))
* **fuse:** guard MIME magic intercept to sz&lt;=16384 to avoid corrupting file copies ([7ec0e7a](https://github.com/rgon/ncrsDesktop/commit/7ec0e7a7e06d9fe5477c5a07cf9199f8c1ad552c))
* **goa:** write keyring credentials before accounts.conf to eliminate auth race ([70e5aa5](https://github.com/rgon/ncrsDesktop/commit/70e5aa578936b4e42f8893a799da8e0e7774cc1b))
* **http3-probe:** require HTTP/3 response version; reqwest 0.13 silently falls back to HTTP/1.1 ([a276b23](https://github.com/rgon/ncrsDesktop/commit/a276b23b2da99518e34e8af65c64208c14c73a0d))
* **http3-probe:** use async reqwest client with dedicated runtime for reliable H3 detection ([1bc9476](https://github.com/rgon/ncrsDesktop/commit/1bc9476609fff125fddfee4fe089ac3643a7a217))
* **http3:** fall back to HTTP/2 when HTTP/3 connection fails in notifications ([3b217c5](https://github.com/rgon/ncrsDesktop/commit/3b217c538d8d510ef0641db9d9cc2aa590970897))
* **keyring:** delete before save and refresh creds on 401 in notification poll ([d9ff15b](https://github.com/rgon/ncrsDesktop/commit/d9ff15bc19cc2c0849079da5233bc08cd24d219a))
* **logout:** preserve server URL in login form after re-login flow ([e58ccaa](https://github.com/rgon/ncrsDesktop/commit/e58ccaa58ba5b789353cac037b02feb1ba372719))
* **net:** retry transient network errors and map timeout/network to ETIMEDOUT/EAGAIN ([bd1e882](https://github.com/rgon/ncrsDesktop/commit/bd1e8826305d7fa48876cf20b9e58f601d4bc410))
* **read:** fail copy immediately when uncached file unreachable instead of serving stale cache ([7ec0e7a](https://github.com/rgon/ncrsDesktop/commit/7ec0e7a7e06d9fe5477c5a07cf9199f8c1ad552c))
* **remount:** unmount active FUSE mount before restarting so mount path changes take effect ([27931d4](https://github.com/rgon/ncrsDesktop/commit/27931d4fca349877e5f8d1896daaeb4c2bf6a632))
* **ui:** show 'Log in' button for auth errors instead of 'Remount' ([3144987](https://github.com/rgon/ncrsDesktop/commit/31449872c6e1295a161e284447713eae0b746f77))
* **webdav:** use Overwrite: T in MOVE so rename atomically replaces existing destinations: WebDAV move had different behaviour to POSIX move. ([d7ccaa3](https://github.com/rgon/ncrsDesktop/commit/d7ccaa3459ae526614d0c9e29f5b08b0ef46e731))


### Performance Improvements

* **fuse:** intercept GLib MIME detection opens with synthetic magic bytes ([08e5673](https://github.com/rgon/ncrsDesktop/commit/08e5673e7ddf7c49f714be670db0b5cbbf190150))
* **fuse:** prefetch small files on open() to parallelise MIME magic-byte detection ([9cc6505](https://github.com/rgon/ncrsDesktop/commit/9cc65059f614f0b03847445a25676bf44a923bd4))

## [0.1.22](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.21...ncrs-v0.1.22) (2026-07-13)


### Bug Fixes

* **ci:** drop unused node/pnpm steps from test job ([e592f89](https://github.com/rgon/ncrsDesktop/commit/e592f8944e334a96f2c244e8d39c7260d45008a9))

## [0.1.21](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.20...ncrs-v0.1.21) (2026-07-13)


### Features

* **calendar:** add nc_calendar plugin with GOA/CalDAV auto-registration ([e58b41e](https://github.com/rgon/ncrsDesktop/commit/e58b41e476d4c232a6a2db4264ceca240ebe8eaa))
* **fuse:** serve MIME type via user.xdg.mime.type xattr to skip GIO content sniffing ([53a1f18](https://github.com/rgon/ncrsDesktop/commit/53a1f18407e7c839cc56318b5a6fad119b263a5e))
* **thumbnailer:** add ncrs-thumbnailer for PDF with evince fallback for local files ([180dc35](https://github.com/rgon/ncrsDesktop/commit/180dc35a14f716b5ab6c47e4836a09da5a897c6a))


### Bug Fixes

* **calendar:** manage own GOA account; never reuse manually-added entries ([7289db0](https://github.com/rgon/ncrsDesktop/commit/7289db015bf6b35c341c3f0f08eded613fc73acd))
* **cicd:** pin pnpm version ([896c25b](https://github.com/rgon/ncrsDesktop/commit/896c25b18d07a5f9feb4a40ca2bd0ae2a704f6ad))
* **ci:** install pnpm via npm instead of pnpm/action-setup ([c270559](https://github.com/rgon/ncrsDesktop/commit/c270559a4cff74dbec5b1d7f90dbd81a5a28a737))
* **preview:** prefetch thumbnails for previewable files lacking server-cached previews ([b09cc6c](https://github.com/rgon/ncrsDesktop/commit/b09cc6c3f8ac1fdf64972f70d8f162ab1ed24193))


### Performance Improvements

* **fuse:** raise attr TTL to 30 s to avoid per-second kernel re-queries ([8fc19cf](https://github.com/rgon/ncrsDesktop/commit/8fc19cfc0ccf3057b4b8e8cb7b41a92a8e417293))
* **fuse:** release cache lock before scanning dir entries in getattr and lookup ([52cf175](https://github.com/rgon/ncrsDesktop/commit/52cf175b68e7308c01224ca64077d5eefe442e44))

## [0.1.20](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.19...ncrs-v0.1.20) (2026-07-12)


### Features

* **preview:** touch FUSE atime after thumbnail write to auto-refresh Nautilus ([5ccd830](https://github.com/rgon/ncrsDesktop/commit/5ccd830948ed9730dcdb0ef172acb0f68ebeff3c))

## [0.1.19](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.18...ncrs-v0.1.19) (2026-07-12)


### Performance Improvements

* **preview:** increase thumbnail throughput; add fetch/convert timing logs ([738fe16](https://github.com/rgon/ncrsDesktop/commit/738fe167957deac999fc3441cd961b98b73064d0))

## [0.1.18](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.17...ncrs-v0.1.18) (2026-07-12)


### Features

* --auto-keep-locally-modified-files flag controls post-upload emblem and local copy ([ef4fba1](https://github.com/rgon/ncrsDesktop/commit/ef4fba1b123b0ad3faa9daaec27ced24a0c56936))
* add 'View in Nextcloud Web' right-click menu via WEBURL IPC command ([12b64cf](https://github.com/rgon/ncrsDesktop/commit/12b64cfd0b9af16a25a196510adcaced66552d5b))
* add bearer_token and auth_command to config ([9cbbc23](https://github.com/rgon/ncrsDesktop/commit/9cbbc2394b0cbf3c92a9d8075158853141513c22))
* add cache cleanup with configurable max size and auto-purge by age ([07b3e8e](https://github.com/rgon/ncrsDesktop/commit/07b3e8e62b6ae4e923abc279a8875f9d7044b503))
* add CHANGES invalidation tracing to Nautilus extension ([69bfa2f](https://github.com/rgon/ncrsDesktop/commit/69bfa2f2fe7614b16fdd244e2aa22c598dba242d))
* add CHANGES IPC command for live emblem refresh after Keep Locally ([1ea8f11](https://github.com/rgon/ncrsDesktop/commit/1ea8f11deec2d9c70197135e39598829bb97c1f0))
* add CLI argument parsing with --offline, --config, and --mount-point flags ([1409a80](https://github.com/rgon/ncrsDesktop/commit/1409a800e2227112ad729f86fc9d86cea0b2b8d6))
* add configurable cache cleanup interval (default 1h) ([f0dfe4e](https://github.com/rgon/ncrsDesktop/commit/f0dfe4eb7b68d6f23561cf1a97e71f477677ca1e))
* add ConflictsView and ErrorsView components ([c3d0a36](https://github.com/rgon/ncrsDesktop/commit/c3d0a3681fc9088798e14af33049e0700c4c9ad4))
* add Credentials auth abstraction for basic and bearer token support ([61d6488](https://github.com/rgon/ncrsDesktop/commit/61d6488015b25078a34ee38d976b2237125f6dcf))
* add deb build script ([8169ea6](https://github.com/rgon/ncrsDesktop/commit/8169ea68646ba68cb717a02f721262113c402e80))
* add exclude_folders config to hide folders from FUSE mount ([78ae3cc](https://github.com/rgon/ncrsDesktop/commit/78ae3ccdd0109dc051465059907aa9d9e3e1ae95))
* add filename validation module matching Nextcloud server rules ([6bcf3d7](https://github.com/rgon/ncrsDesktop/commit/6bcf3d7cbd66a005edd5d2d07fb0710730443980))
* add frontend plugin registry and plugins view ([549cefe](https://github.com/rgon/ncrsDesktop/commit/549cefec842dbfe7e6de870345b41c4cefa7d578))
* add fuse_notify and mutation_journal modules ([07adcf2](https://github.com/rgon/ncrsDesktop/commit/07adcf2237c526cbe664dee679ae352884120315))
* add GNOME Shell search provider for server-side Nextcloud search ([29e4595](https://github.com/rgon/ncrsDesktop/commit/29e4595540f8dfe53224a961f15434b51960fbae))
* add info-level tracing across FUSE, IPC, PROPFIND and Nautilus extension ([1d77751](https://github.com/rgon/ncrsDesktop/commit/1d7775197b86b63c23a1f666e200cb7405b2ecbe))
* add keep_paths config to auto-keep remote paths on startup ([4cdf9a8](https://github.com/rgon/ncrsDesktop/commit/4cdf9a870a3fc55638592cfb61095e7a186447ae))
* add logging ([5fe7983](https://github.com/rgon/ncrsDesktop/commit/5fe79837e2248cf75d3265e77873ba18a7af0a9e))
* add Nautilus columns for sync, sharing, permissions, owner, and size ([357a3fd](https://github.com/rgon/ncrsDesktop/commit/357a3fd3cb391c550986c04d2f22285a7f72b32f))
* add nc_passwords plugin crate with API client ([3d061f3](https://github.com/rgon/ncrsDesktop/commit/3d061f38f0a2b37e232003e5dcbd274fd68963c3))
* add nc:// URI handler for Nextcloud Edit Locally support ([fa604b4](https://github.com/rgon/ncrsDesktop/commit/fa604b48ec945ad6e98b608ce520a8e3fa6dac50))
* add ncrs_plugin shared crate with plugin trait ([8a31352](https://github.com/rgon/ncrsDesktop/commit/8a31352e1bfd1a4840d204c585f043a89a431371))
* add Nextcloud Notify Push WebSocket client for real-time cache invalidation ([5ca11cf](https://github.com/rgon/ncrsDesktop/commit/5ca11cf565f39f8e5c7c48e3952391d69660a633))
* add Nextcloud OCS notifications API to ncrs_core ([e7af4e9](https://github.com/rgon/ncrsDesktop/commit/e7af4e9ea9f1544570124f43fde07920af717644))
* add Nextcloud unified search API to ncrs_core ([b538ea5](https://github.com/rgon/ncrsDesktop/commit/b538ea5b60b3097a896426722d7ee579e9092ad6))
* add oc:permissions and oc:fileid to PROPFIND properties ([d0017d2](https://github.com/rgon/ncrsDesktop/commit/d0017d25353e115bfe246aadefd7f91b4279614f))
* add offline resilience with connectivity monitor and cache-only fallback ([747b9c9](https://github.com/rgon/ncrsDesktop/commit/747b9c98416c1c948bf6c8693032a4c6dfb96547))
* add PasswordsView frontend and register in plugin UI ([07e33de](https://github.com/rgon/ncrsDesktop/commit/07e33deef1f862b1517db0b54caee575ccbe2c17))
* add plugin registry and tray integration ([06a5cf9](https://github.com/rgon/ncrsDesktop/commit/06a5cf96bfdba4ec3f17feb68401d915e80fd4b1))
* add provider type filtering to search ([d77319f](https://github.com/rgon/ncrsDesktop/commit/d77319f68c9f1ab9a64c3c1a99d3ba1bfa02d8e5))
* add resolve_fileids() WebDAV SEARCH by fileid ([a13eaa0](https://github.com/rgon/ncrsDesktop/commit/a13eaa04566741592fd35a62566218b2c6e2e67b))
* add SEARCH IPC command and Nautilus search dialog for Nextcloud ([38e6019](https://github.com/rgon/ncrsDesktop/commit/38e6019875bcdab86e2e0ee0cf1050338951a043))
* add search_nextcloud Tauri command with parallel provider queries ([3aee79a](https://github.com/rgon/ncrsDesktop/commit/3aee79ab112108f695895663a80d3f011d4c492e))
* add SearchView component with debounced NC unified search ([f41eb99](https://github.com/rgon/ncrsDesktop/commit/f41eb993c9551d21e98e241ef65a7f4eab9c9f22))
* add systemd user service and deb packaging metadata ([7b7333b](https://github.com/rgon/ncrsDesktop/commit/7b7333b05ba2d90bc27d1967f5df75e98b677377))
* add WebDAV write operations with FUSE callbacks and ETag conflict resolution ([bdc122a](https://github.com/rgon/ncrsDesktop/commit/bdc122afd63032d03321107a0c2ff11ec42d15ce))
* add window-context detection for auto-suggesting passwords based on active browser tab ([ea1c5ec](https://github.com/rgon/ncrsDesktop/commit/ea1c5ec0e13f3aba2d5622229d7121ad7d74aa4b))
* **auth:** Nextcloud Login Flow v2 — trigger when password missing ([f8293e7](https://github.com/rgon/ncrsDesktop/commit/f8293e76f4337915c3183c547e1d5ffe72b819f3))
* **auth:** system keyring for app password; warn on insecure config perms ([35e074b](https://github.com/rgon/ncrsDesktop/commit/35e074b5377e9ed53a971ba91fc542fb9795c35d))
* auto-create mountpoint if doesn't exist ([9de011b](https://github.com/rgon/ncrsDesktop/commit/9de011bff871f1a25ca7191c8493a71e0cdc9666))
* background file caching on open, keep-locally IPC+menu, fix emblems, add debug logging ([f792818](https://github.com/rgon/ncrsDesktop/commit/f79281806ebbe772fb60f5940303276bacc7413e))
* background notification polling and dismiss/get Tauri commands ([9ad4fb5](https://github.com/rgon/ncrsDesktop/commit/9ad4fb529ec9d8ea44258faf2171fe3d44b22e8c))
* cache directory ls and update with notify-push ([415a87f](https://github.com/rgon/ncrsDesktop/commit/415a87fd0ef0208c544e90eb9bbc13a12cba7e04))
* cache downloaded files to disk, invalidate by modified time ([ee9b54d](https://github.com/rgon/ncrsDesktop/commit/ee9b54d9abb9159773ac05cc7e6b352f14371a44))
* cache streaming reads to disk when full file is covered, with configurable read-ahead ([54b441d](https://github.com/rgon/ncrsDesktop/commit/54b441d9f1c707ab44d89d5c8ddce2d06dc19e5c))
* **ci:** build the deb via build-deb.sh and verify it with test-deb.sh ([6eadd86](https://github.com/rgon/ncrsDesktop/commit/6eadd86600697fbc195d7cb042509c786ca5683a))
* **config:** enable http3 by default; add explanatory comments to advanced settings ([4b34dbc](https://github.com/rgon/ncrsDesktop/commit/4b34dbc3a33bfefb601c0313ea63d4501648848a))
* **core:** add --print-default-config flag and explicit ncrs bin target ([4b9dd9d](https://github.com/rgon/ncrsDesktop/commit/4b9dd9ddad8906915c21ebf4f5eb90c5694803aa))
* **core:** add STATE, PAUSE and RESUME IPC verbs for external clients ([3b0bb03](https://github.com/rgon/ncrsDesktop/commit/3b0bb034f761c1bf4e6981ec13e9872bf4065ae3))
* deferred subdirectory readdir, partial-download icons, evict command, cache recovery, and fix web URLs ([a4c2a2b](https://github.com/rgon/ncrsDesktop/commit/a4c2a2b91dfcc91a6a866ee3fe3f10da1046db5e))
* derive FUSE mode bits from Nextcloud oc:permissions flags ([32f7be3](https://github.com/rgon/ncrsDesktop/commit/32f7be37b623cd87bed5ccd34eccaca9652012a7))
* derive Serialize/Deserialize for DavEntry for cache persistence ([7f8bc65](https://github.com/rgon/ncrsDesktop/commit/7f8bc65c9ea3f1e12ea5f06c5a0dcfa55f64bb98))
* detect shared files via oc:share-types and show shared emblem in Nautilus ([22d65df](https://github.com/rgon/ncrsDesktop/commit/22d65df9bbbcb7eae2e80e4e8cee1e29538c0f64))
* dir cache persistence, sync reads, HTTP/3 client, child prefetch batching ([1ced20d](https://github.com/rgon/ncrsDesktop/commit/1ced20d4187746245546bb379d58b47a00466aaa))
* expose error_log, transfer_map, and journal through Tauri state and commands ([ab5a4e4](https://github.com/rgon/ncrsDesktop/commit/ab5a4e459c60275c033c5ee4b99fa1c2f5cd266c))
* extend SyncProgressView with transfers and error indicators ([609512d](https://github.com/rgon/ncrsDesktop/commit/609512dac488fa72c96e864e6eaaab76c8d359c6))
* **fuse:** auto-delete stale GIO atomic-write temps from server on readdir ([e02bbfd](https://github.com/rgon/ncrsDesktop/commit/e02bbfd80fd5a2a05e5c75ccef51604a002b6e3e))
* ghost entries + inotify events for all FUSE ops with Nautilus DBus reload ([8003ed4](https://github.com/rgon/ncrsDesktop/commit/8003ed4e8780dfd6ca77de0003e7f33c4e5e0c15))
* **gui:** add description hints to advanced settings toggles ([a129dcb](https://github.com/rgon/ncrsDesktop/commit/a129dcbc4b447653e11b97f4919e37dd0d860ffc))
* **gui:** add Remount button to settings footer ([3838009](https://github.com/rgon/ncrsDesktop/commit/38380091e36837c784b5a39bdf6ac86cc73b9741))
* **gui:** add settings view with yaml config editing and version indicator ([63b3154](https://github.com/rgon/ncrsDesktop/commit/63b315440767e394c001b970a35a8c121296188c))
* **gui:** attach to a running daemon over IPC instead of mounting a second time ([0f9c804](https://github.com/rgon/ncrsDesktop/commit/0f9c804a64bfa21507af1ef52ab641d54afddc09))
* **gui:** left-click tray icon opens window, right-click shows menu ([09d9295](https://github.com/rgon/ncrsDesktop/commit/09d92952867910e1d16d69c87ebc33a6887fbf64))
* **gui:** mirror journal and conflicts from daemon in attach mode ([6e43466](https://github.com/rgon/ncrsDesktop/commit/6e43466534b63a690d65732d401b0638369af321))
* handle FUSE unmount event with shutdown flag, GUI remount ([e6238f8](https://github.com/rgon/ncrsDesktop/commit/e6238f8764f1191a9e191837b853c149971ca4ac))
* HTTP Range partial reads with 2MB read-ahead buffer for streaming ([9a10ebc](https://github.com/rgon/ncrsDesktop/commit/9a10ebcbfd424280a4b5f86bd65fc90f760f3bb1))
* **http:** probe HTTP/3 at startup, fall back to HTTP/2 if QUIC unavailable ([8adc9b2](https://github.com/rgon/ncrsDesktop/commit/8adc9b22765a3b51f5449f33458c333b57ac5b1e))
* implement base features in UI, match most NC Desktop components. Open in top right corner (hack for Wayland) ([68c39eb](https://github.com/rgon/ncrsDesktop/commit/68c39eb286b7358e19bd2fe4ae809c9fbfff04bb))
* implement basic configuration parser ([dc012ee](https://github.com/rgon/ncrsDesktop/commit/dc012ee6bfdc9fb6fedc90b85fa1127a78a82f2f))
* implement basic Tauri UI skeleton with tray icon and menu ([2abaeea](https://github.com/rgon/ncrsDesktop/commit/2abaeea12bb6965f4d6214f3fc6a2488e18ff5a2))
* implement chunked uploads for files larger than 10MB ([a215449](https://github.com/rgon/ncrsDesktop/commit/a2154494fa9295e796d54d318efda65103d31de3))
* implement pause sync via shared paused flag in core and GUI ([a8f3134](https://github.com/rgon/ncrsDesktop/commit/a8f31347810fe9413efc0e6abfa84118f408fa36))
* incremental readdir with channel-based streaming for cold cache ([01c3736](https://github.com/rgon/ncrsDesktop/commit/01c37366d03483fc841e6bc86e55a409adc1f32a))
* **ipc:** add DETAILDIR batch command for directory metadata ([f15d22d](https://github.com/rgon/ncrsDesktop/commit/f15d22d46fb7720265b40237d3c56ea5f5c8dce8))
* **ipc:** add DETAILDIR to batch a directory's child metadata into one reply ([e1eb509](https://github.com/rgon/ncrsDesktop/commit/e1eb509cc744703b1471c691bdb4a5a99462d90c))
* **ipc:** add VERSION handshake so daemon/extension protocol mismatch is logged ([8e13f37](https://github.com/rgon/ncrsDesktop/commit/8e13f37b93bd19725eb183b8bab709d9da1050cf))
* **issues:** add Clear all button for conflicts in the warnings tab ([4a49942](https://github.com/rgon/ncrsDesktop/commit/4a499429f66362f0da9ec77f3cfca26b44166f3e))
* **issues:** add per-item error dismiss button ([fe5fde9](https://github.com/rgon/ncrsDesktop/commit/fe5fde94a559e80593219c983610c65e6795c343))
* load config from ~/.config/ncrs/config.yaml, create skeleton on first run ([cb7ca65](https://github.com/rgon/ncrsDesktop/commit/cb7ca65078b030402e57197ff8215fde46b3be29))
* **login:** pre-fill server URL from existing config ([bd7d003](https://github.com/rgon/ncrsDesktop/commit/bd7d003ed4c7ac9a7ad14870dcb79561f8751c49))
* nautilus extension for file sync status emblems ([b63b021](https://github.com/rgon/ncrsDesktop/commit/b63b0217d494079b0ba14a5fffe8f926261d6573))
* open file results in file browser with view-online button ([d844d80](https://github.com/rgon/ncrsDesktop/commit/d844d8013f340d4d47f2087169330d0673b7af97))
* **packaging:** ship GUI desktop entry, icons, autostart and example config in the deb ([e399413](https://github.com/rgon/ncrsDesktop/commit/e39941328fdf3657952b469d35c9166712a6bfb5))
* populate IPC detail for directories themselves via PROPFIND self-entry ([b7b0bb8](https://github.com/rgon/ncrsDesktop/commit/b7b0bb80ee39ba4fc379cf5fd3e850a15cb9ad17))
* prefetch NC preview thumbnails into XDG cache on directory listing ([16ed616](https://github.com/rgon/ncrsDesktop/commit/16ed6168a2532018df6d89bbc681aac276b09514))
* **preview:** fetch thumbnails for RAW camera formats regardless of has_preview flag ([98a92cf](https://github.com/rgon/ncrsDesktop/commit/98a92cf03e6bcd3b0c5853d8a92d2bf189bf5e1c))
* proactively propfind invalidated etags of /* at boot ([18a9e2e](https://github.com/rgon/ncrsDesktop/commit/18a9e2eedf9e1d1a0cc62ef85bb64f257af22a49))
* raw PROPFIND with NC properties (has-preview, etag, oc:size), replace remotefs for listings ([c78fd46](https://github.com/rgon/ncrsDesktop/commit/c78fd468d0ef57aa3a0cde2423cd2a204a9da4da))
* redesign PasswordsView as quick-access popup with click-to-copy ([6a28378](https://github.com/rgon/ncrsDesktop/commit/6a283789f1e2146f429a9dafcfd67afbcf92a8d8))
* register nc_passwords plugin in tauri backend ([853a457](https://github.com/rgon/ncrsDesktop/commit/853a45791512e0b1ca16975eaaeb7a824e39063b))
* replace dummy notifications with real NC API data, avatar, and dismiss ([ebc156b](https://github.com/rgon/ncrsDesktop/commit/ebc156b11cff81e82d8ef06833a501d575df3c60))
* replace stub 'Add account' with working Log out button ([c16bf42](https://github.com/rgon/ncrsDesktop/commit/c16bf42bab588e16e099f484e389e65d686a9548))
* send OS notifications ([6f6467a](https://github.com/rgon/ncrsDesktop/commit/6f6467ace26d3befb1a11f74e95f1cb1fa949ad0))
* separate kept and cached files into distinct directories with per-file status ([9157a48](https://github.com/rgon/ncrsDesktop/commit/9157a485777ecc44fb60891608f39da04569db10))
* set x-gvfs-notrash mount option to prevent Nautilus trash dirs on Nextcloud ([6b10f5c](https://github.com/rgon/ncrsDesktop/commit/6b10f5cb83f5b231eca1e550289a43f61c86ed2f))
* **settings:** add GNOME GIO intermediate auto-cleanup toggle ([7fa2692](https://github.com/rgon/ncrsDesktop/commit/7fa269218da1f2c84aa6692e0cd21d165dd2e645))
* show correct size and uploading emblem for newly written files ([b65f7f0](https://github.com/rgon/ncrsDesktop/commit/b65f7f0994b49d8f7922b189174b82aae027657a))
* show storage usage in GUI for kept files, cache, and server quota ([8c8b207](https://github.com/rgon/ncrsDesktop/commit/8c8b207ede8bd8c5e13c48a9e266b73ec9074c0f))
* stale-while-revalidate for directory cache, serve cached listings immediately ([96d6c20](https://github.com/rgon/ncrsDesktop/commit/96d6c2029b19fd5985b7aa85c7e7119df616e9ca))
* support Nextcloud remote wipe to delete local data on server command ([0dd7c38](https://github.com/rgon/ncrsDesktop/commit/0dd7c386937e5c6dfa28b904aeb92a0c07a9e292))
* **tauri:** add dismiss_error command to remove single error by timestamp ([4e71616](https://github.com/rgon/ncrsDesktop/commit/4e7161609e16ec46dda2ca4cbcfe27b0340ab9a9))
* **theme:** fetch Nextcloud server accent color from capabilities and apply to UI ([0371c1b](https://github.com/rgon/ncrsDesktop/commit/0371c1b56610140522073c8db12250be99520afc))
* thread http3 flag through notification and search clients ([f9ad7fe](https://github.com/rgon/ncrsDesktop/commit/f9ad7fe2ec9cbe8a3038f5fd544b43e68daaed62))
* **thumbnailer:** add CR3/CR2 raw thumbnail support via embedded JPEG preview ([0c39c1a](https://github.com/rgon/ncrsDesktop/commit/0c39c1ad47f2c45fa5127d2894a2184fd8129c5c))
* **thumbnailer:** fetch NC preview via IPC instead of reading local raw file, expand to all registered RAW MIME types ([b5d44c4](https://github.com/rgon/ncrsDesktop/commit/b5d44c48cedc9394d9c4cfe22ce56a1975406de2))
* **ui:** detect system dark/light mode and add design tokens ([0f244f5](https://github.com/rgon/ncrsDesktop/commit/0f244f569eb577130f6abd3bea6d13d66c321fc7))
* unix socket IPC server for file sync status queries ([1c2c86f](https://github.com/rgon/ncrsDesktop/commit/1c2c86fbb658409197ffa52d6143880a275b46ff))
* update nautilus extenision on runui ([b752dfa](https://github.com/rgon/ncrsDesktop/commit/b752dfad782d8ab477a18aadadebc7ed4f4b1545))
* upgrade reqwest 0.11 to 0.12 with HTTP/3 QUIC support ([ec801f1](https://github.com/rgon/ncrsDesktop/commit/ec801f12fee69d808ce6cb425b6b42336b6328d6))
* wire errors, transfers, and conflicts state into main page ([6e67051](https://github.com/rgon/ncrsDesktop/commit/6e67051d7bf94e2272be15a729b2e1d2919031ca))
* wire tauri commands to real backend state and config ([0ce8b89](https://github.com/rgon/ncrsDesktop/commit/0ce8b891679b71dce7312f75fec10c66b8c1ea43))


### Bug Fixes

* add done flag to stream buffer so waiters fail fast on short downloads ([e5205a0](https://github.com/rgon/ncrsDesktop/commit/e5205a029fd1153ba506e205f63e0f6abaf9508a))
* batch-populate IPC maps before readdir reply for immediate column data ([fc322f2](https://github.com/rgon/ncrsDesktop/commit/fc322f2a17405779ffda4973a8816d03363450d0))
* bound IPC connections to 64 with 60s read timeout to prevent thread leaks ([5917299](https://github.com/rgon/ncrsDesktop/commit/5917299fdb22c20cdebe5240b2904ba323006b04))
* **build:** sync Cargo.lock to workspace version 0.1.12 ([6ac52f7](https://github.com/rgon/ncrsDesktop/commit/6ac52f7590988baf7f4da5b18daeb1a5a0c098eb))
* **cache:** delete orphaned write_* staging files after upload completes ([af1686b](https://github.com/rgon/ncrsDesktop/commit/af1686b3b4dcc2786c8bfeff1c59d51870b6e971))
* cancel previous download when new range read starts for same fh ([7177ee4](https://github.com/rgon/ncrsDesktop/commit/7177ee4a30af799771151e6af813b7cd390a1a7f))
* check dir cache before PROPFIND in keep-locally to avoid querying file paths as directories ([87a4e68](https://github.com/rgon/ncrsDesktop/commit/87a4e689a368096ac494ac4e53d11112cf242983))
* **cicd:** proper release-please config ([9798a78](https://github.com/rgon/ncrsDesktop/commit/9798a78c30d8ed21183b734e12a809bfb193599a))
* **ci:** pass token explicitly to release-please action ([3b2ce31](https://github.com/rgon/ncrsDesktop/commit/3b2ce3199eecf0b1627815d8672befb075ee4fd2))
* clean up stale zero-byte write_* temp files on daemon startup ([cdb1599](https://github.com/rgon/ncrsDesktop/commit/cdb1599b1c2c8ad04073b8ed15731531ef73d49b))
* **core:** parallelize boot validation and update detail maps on background dir refresh ([553b5fe](https://github.com/rgon/ncrsDesktop/commit/553b5fea7f0481eed6128048fa0a6097c4f16e88))
* **core:** refuse to mount over a live mount or non-empty dir, remove mount dir on exit ([d9a6412](https://github.com/rgon/ncrsDesktop/commit/d9a6412113f1f5194b4f96d9e1a8259161d9a720))
* **core:** remove erroneous MKCOL 409 idempotent arm that silently dropped journal entries ([16d81e7](https://github.com/rgon/ncrsDesktop/commit/16d81e7438ba65c92462755becf5dae0ef5ddf02))
* **core:** surface PROPFIND auth/network errors to readdir and GUI error log ([1c06516](https://github.com/rgon/ncrsDesktop/commit/1c065164e60df6aafa3b8b857958114814255b7b))
* **core:** validate mount point before touching the IPC socket and shared state ([f196be8](https://github.com/rgon/ncrsDesktop/commit/f196be82d76580cd9f0b520f0a33d9487af817a1))
* correct Nextcloud oc:permissions flag mapping in Nautilus extension ([6685d6f](https://github.com/rgon/ncrsDesktop/commit/6685d6fa1f2745add45b7324a068a0de350333ac))
* dirty file path after PUT so Nautilus clears uploading emblem automatically ([2f39435](https://github.com/rgon/ncrsDesktop/commit/2f39435dae58178ef3a65549bca495bcba0af5e6))
* don't invalidate directories on boot if etag not changed, better atime notify-push ignore after our own propfind to prevent infinite loops ([7e5074a](https://github.com/rgon/ncrsDesktop/commit/7e5074a725ef5a1a5237d6a2885bd8dbd5444373))
* drop AutoUnmount — fuser 0.17 requires allow_other with auto_unmount ([8d2da94](https://github.com/rgon/ncrsDesktop/commit/8d2da94898881c26d4fe7cbdc0955e481ce2e511))
* enable rustls-tls for tungstenite and retry notify_push discovery on failure ([871f45e](https://github.com/rgon/ncrsDesktop/commit/871f45e898be7295540f9dab1720c76c2c9cc830))
* enforce Nextcloud oc:permissions via DefaultPermissions FUSE mount option; add perms_to_mode tests ([ced8a5c](https://github.com/rgon/ncrsDesktop/commit/ced8a5c67b9bc4b5b3a9b349847b230a7e894288))
* fetch parent directory on lookup cache miss after daemon restart ([a79b075](https://github.com/rgon/ncrsDesktop/commit/a79b0755dc50f799f6f3472fde9c5ca71f43b325))
* **fuse:** delete all GIO temps on PROPFIND, not just age-threshold ones ([921d2a7](https://github.com/rgon/ncrsDesktop/commit/921d2a77d076aede28f65c00b91fdfaf21ab86a1))
* **fuse:** guard newly created files in uploading set to prevent ENOENT race ([9462955](https://github.com/rgon/ncrsDesktop/commit/94629552fc2c6551995b9edfca4a413cd595ed9b))
* **fuse:** preserve in-flight uploads during concurrent dir cache PROPFIND refresh ([c41f68e](https://github.com/rgon/ncrsDesktop/commit/c41f68eae36ca25a0588f2585ce6893f084c650e))
* **fuse:** update inode map on rename so saved files don't vanish ([233b5db](https://github.com/rgon/ncrsDesktop/commit/233b5dbf63c62a4faacee613d01247762ef90113))
* gate child-dir PROPFIND prefetch behind aggressive_prefetch and add HTTP request throttle ([5ec0d5b](https://github.com/rgon/ncrsDesktop/commit/5ec0d5b29e682d5a0bb592f4ff2aabd17bd4b2ea))
* green-checkmark after upload; NC properties appear via forced PROPFIND ([f42c403](https://github.com/rgon/ncrsDesktop/commit/f42c4030e517ddc2541be70165bb8a72ec560063))
* guard unlink/rmdir/rename with NC D/N/V flags; block delete in create-only shared dirs ([476a2bd](https://github.com/rgon/ncrsDesktop/commit/476a2bd7bcf728e72b772dac56e8100f1a8a1f69))
* **gui:** embed tray icons at compile time and enforce a single app instance ([1afc89d](https://github.com/rgon/ncrsDesktop/commit/1afc89d6cbc519ec435f558bd1ff9b4e29c72ab8))
* **gui:** pin plugin bare imports to local node_modules for production builds ([e49e969](https://github.com/rgon/ncrsDesktop/commit/e49e969dd98a28927cd41757463838be5d1965e1))
* **gui:** prevent floating panel from shrinking on HiDPI-scaled displays ([80161ed](https://github.com/rgon/ncrsDesktop/commit/80161ed571ae6cb57b4d9ad4f13768d00d44ba49))
* **gui:** regenerate app icons from the ncrs brand mark instead of the tauri template ([c87d7ef](https://github.com/rgon/ncrsDesktop/commit/c87d7efa2aefaead42fb059314a085d2613d4089))
* **gui:** use composedPath for click-outside detection of detached nodes ([74922f5](https://github.com/rgon/ncrsDesktop/commit/74922f5d2ada957fbcc80bdff1fc04fc86d19aae))
* harden bearer auth — redact secrets, stream downloads, pre_auth WS, validate creds ([eccecbe](https://github.com/rgon/ncrsDesktop/commit/eccecbe238bc19f00006d2b41eda98c1892ad768))
* harden FUSE, IPC, and Nautilus extension against panics and errors ([40aee6e](https://github.com/rgon/ncrsDesktop/commit/40aee6e3bb78320b7830da0a71872b4587389d1a))
* **http:** correct misleading http3 alt-svc comment and hint ([dfd851c](https://github.com/rgon/ncrsDesktop/commit/dfd851c8edf2558d22f954859c33d688497ed2b5))
* **http:** remove http3_prior_knowledge; use alt-svc negotiation instead ([239809c](https://github.com/rgon/ncrsDesktop/commit/239809ccbe2a97b1fd92fb6a48237ad2a62c441b))
* **issues:** override DaisyUI grid on alert cards, add dismiss × button ([b1a260f](https://github.com/rgon/ncrsDesktop/commit/b1a260f295e78db17a96f8e49e2fb4e275fcc9c4))
* **issues:** replace DaisyUI btn with plain icon-button classes to fix overflow ([9671b18](https://github.com/rgon/ncrsDesktop/commit/9671b182b1f3951d0544d4c891e64d2b216f0aa7))
* lazy-unmount stale FUSE mount before remounting on restart ([bd4cec5](https://github.com/rgon/ncrsDesktop/commit/bd4cec563fc25f0e1ffba4beb431f39039084fa4))
* **login:** correct init endpoint to /index.php/login/v2 and strip WebDAV paths from user input ([4410f91](https://github.com/rgon/ncrsDesktop/commit/4410f91c3931b208fcdf161903dd1f6fbfb4c0e2))
* **login:** send User-Agent header so Nextcloud shows app name in OAuth grant page ([e7e9320](https://github.com/rgon/ncrsDesktop/commit/e7e9320e5a59bd708e31f3eba39f835bd11b216f))
* make search async with cancellation and debounce throttling ([5ac2743](https://github.com/rgon/ncrsDesktop/commit/5ac2743cf20ba0cb3e098edf43db34175366bb80))
* **mount:** remove IPC socket on FUSE teardown so remount doesn't enter attach mode ([bf5c7d5](https://github.com/rgon/ncrsDesktop/commit/bf5c7d553e5ee07fe4a10c2c4f595b37828876c7))
* **mount:** surface FUSE errors to UI and allow remount from error state ([9aa6217](https://github.com/rgon/ncrsDesktop/commit/9aa6217e0842bb4a3355effe93c56bffc9efb236))
* move DETAIL logic into update_file_info_full which Nautilus 4 actually calls ([c61c494](https://github.com/rgon/ncrsDesktop/commit/c61c49420f28251038360dccd5988657c0ad4c88))
* **nautilus:** clamp _poll_skip to 0 to prevent negative value if pool resets it mid-decrement ([38c80aa](https://github.com/rgon/ncrsDesktop/commit/38c80aad3e40e8abbafdc767380d64e8f75cf15f))
* **nautilus:** log malformed FILE_CHANGES entries; set _poll_skip before clearing _poll_running ([e0f1562](https://github.com/rgon/ncrsDesktop/commit/e0f1562890e2e4d1cca2efb46d29ecf287f57a17))
* **nautilus:** refuse dev install when the packaged extension copy exists to avoid GObject type collision ([b3ae4f9](https://github.com/rgon/ncrsDesktop/commit/b3ae4f9a5caf76d299e16fbc4b2dc2246b4ea494))
* never return empty readdir for deferred dirs, debounce dir cache saves ([bc5bc20](https://github.com/rgon/ncrsDesktop/commit/bc5bc20ca94d0351dc71d9bd940d989c43741e32))
* only prefetch subdirs/thumbnails on first readdir, deduplicate prefetch PROPFINDs ([4f5db18](https://github.com/rgon/ncrsDesktop/commit/4f5db180af9b71d6d573fa1db9dd0363d677f5fe))
* **packaging:** build GUI with custom-protocol so the deb embeds the frontend ([3f8c706](https://github.com/rgon/ncrsDesktop/commit/3f8c706a565a70d5308bcab9d8e8ba092585a601))
* parse statusCode as string to match Nextcloud Passwords API response ([25cc8bc](https://github.com/rgon/ncrsDesktop/commit/25cc8bcdef1ce3ffa5954e5af86f9eb33e3911f2))
* percent-decode paths from remotefs-webdav list_dir results ([84d13c7](https://github.com/rgon/ncrsDesktop/commit/84d13c740af5a84361daad3da230c6972a3de69d))
* percent-decode search result titles and paths ([ff294c1](https://github.com/rgon/ncrsDesktop/commit/ff294c1406a1921236fbea9473cc9b2bbdf26542))
* populate IPC maps from getattr/lookup, serve from file_cache in read, move poll off main thread ([29ae52c](https://github.com/rgon/ncrsDesktop/commit/29ae52cde2db51d2a38fb23461741d10ca15d4a4))
* prevent concurrent PROPFIND race in get_or_list_dir ([5d1a506](https://github.com/rgon/ncrsDesktop/commit/5d1a506648dcb423548c3730557920d2e3e83bfb))
* prevent recursive LOG IPC calls, increase socket timeout and recv buffer ([cd57209](https://github.com/rgon/ncrsDesktop/commit/cd57209e5f1489bb1c677f23c34ae4a19fa57b5e))
* **preview:** avoid Cow allocation in fetch_preview_bytes; preallocate PNG buffer; align probe with daemon params ([8376a3c](https://github.com/rgon/ncrsDesktop/commit/8376a3c85eafd7c1fc15fd93235ad8ce978dd241))
* **preview:** convert NC JPEG preview response to PNG for XDG thumbnail cache ([f19f259](https://github.com/rgon/ncrsDesktop/commit/f19f2599d948b45c481716207dbee010bdeada1b))
* **preview:** evict XDG fail-cache entries when thumbnail is written ([c662680](https://github.com/rgon/ncrsDesktop/commit/c662680414452afd58b19b7000786275522261c5))
* **preview:** guard thumbnail_callback with thumb_inflight to prevent duplicate NC fetches ([6c526bc](https://github.com/rgon/ncrsDesktop/commit/6c526bc7e67370f18e578e4abee19ac9cafaf7b5))
* **preview:** percent-encode file: URIs so XDG thumbnail hashes match Nautilus ([ed1a42d](https://github.com/rgon/ncrsDesktop/commit/ed1a42d796349797052ff15d28f37320fa198c14))
* **preview:** throttle on-demand RAW thumbnail fetches to avoid starving FUSE HTTP workers ([8473f2b](https://github.com/rgon/ncrsDesktop/commit/8473f2b5561834a7d88d7f690278ac8d36e345a4))
* reduce Keep Locally concurrency to 2 with yield to avoid Nautilus freeze ([708e4f5](https://github.com/rgon/ncrsDesktop/commit/708e4f547da884bc32a9da3b4eb4539371fd471c))
* reduce thumbnail batch to 4, add 200ms inter-batch and 500ms initial delay ([766fa25](https://github.com/rgon/ncrsDesktop/commit/766fa25b50a23e520ca408d521db543c84063ca5))
* reject empty bearer_token, prevent notification poll from killing other pollers ([f18f696](https://github.com/rgon/ncrsDesktop/commit/f18f6963d45d010945f3840235724315c2138253))
* **release:** annotate workspace version for release-please and unify plugin versions ([d818b7e](https://github.com/rgon/ncrsDesktop/commit/d818b7e79ada55ba356752b2f8f38ee95d8c5a66))
* **release:** use generic extra-file updater for workspace Cargo.toml ([6d0ebb2](https://github.com/rgon/ncrsDesktop/commit/6d0ebb2ee4482e357ee041b4188b523d9c662744))
* **release:** use rust release type so release-please updates workspace Cargo.toml version ([9200368](https://github.com/rgon/ncrsDesktop/commit/920036846d857197608f4755f24b6f74ad45fb43))
* remove dbus reload, let inotify events handle per-file nautilus updates ([fde890d](https://github.com/rgon/ncrsDesktop/commit/fde890d2964964e41a7e9ffd5cdbfd3d6793f92c))
* remove update_file_info stub that blocked async update_file_info_full ([3d32df1](https://github.com/rgon/ncrsDesktop/commit/3d32df1cc9401e5249f26b0f27277331bf4a21ae))
* replace hard cancel with soft self-cancel, limit read throttle to 3 ([4ce0496](https://github.com/rgon/ncrsDesktop/commit/4ce049623652c19feb15f667c47bcd1318351861))
* resolve $plugins alias with absolute path and add plugin loading diagnostics ([a660c06](https://github.com/rgon/ncrsDesktop/commit/a660c06353ea6eb40f06db9246cbde9aa0cc3757))
* restore update_file_info stub, add DETAIL_ASYNC tracing ([ba29455](https://github.com/rgon/ncrsDesktop/commit/ba29455c7a5f68d25980b3d412e7801738bd927c))
* return COMPLETE synchronously for non-mount files to avoid Nautilus async overhead ([0389ab8](https://github.com/rgon/ncrsDesktop/commit/0389ab86025d64c2e282b3dbcafe1ef874a2793d))
* rewrite nautilus extension with non-blocking async update and tests ([178214c](https://github.com/rgon/ncrsDesktop/commit/178214cd812b124a86007868a3f9df086723b04c))
* **security:** chmod 0600 config file to protect app password ([5ca3337](https://github.com/rgon/ncrsDesktop/commit/5ca3337ed7df7cb2d897d770698f08fe9a5e370c))
* send self-entry immediately via channel, mark paths dirty after IPC population ([29c5a98](https://github.com/rgon/ncrsDesktop/commit/29c5a98482ae6766332ef2edf502d57303dbfdb4))
* **settings:** add gap between toggle label and checkbox ([a28ad2e](https://github.com/rgon/ncrsDesktop/commit/a28ad2eff626f0b0fa33a8c4418092f9832e233f))
* skip background download for streaming media, show blue emblem during active downloads ([94452e5](https://github.com/rgon/ncrsDesktop/commit/94452e5deb7b5143cbf19c17cecc0128f49bfd9d))
* skip IPC socket queries for files outside ncrs mount point ([eefdd6d](https://github.com/rgon/ncrsDesktop/commit/eefdd6d8d6701f3e3a2b1e025cacb61794ef156d))
* skip zero-byte cached files and clean up failed downloads ([e1f8c4a](https://github.com/rgon/ncrsDesktop/commit/e1f8c4a247a41360a101fc4ac2f2bb948c6a5ff6))
* stop eager full-file download on open, use fileId for preview API ([6c60c37](https://github.com/rgon/ncrsDesktop/commit/6c60c37e92d4a146bcf685dce5d04cbdfb7af5c3))
* suppress notify_push self-notification loop via ETag pre-check ([f5350b1](https://github.com/rgon/ncrsDesktop/commit/f5350b10b9f14a4764c6ed7c5e598b033486c94e))
* suppress self-notify kernel dentry invalidation for freshly-fetched dirs ([e492b37](https://github.com/rgon/ncrsDesktop/commit/e492b37325353ff32925a7f23a449ccc41f0bc9b))
* **thumbnailer:** catch OSError from missing exiftool; use or-fallback for XDG_RUNTIME_DIR ([7c42d8d](https://github.com/rgon/ncrsDesktop/commit/7c42d8de732540d7dcc9df8cec9d9f12cf354ee9))
* **thumbnailer:** route by is_remote instead of is_local so slow/absent daemon falls through to exiftool ([881bcf3](https://github.com/rgon/ncrsDesktop/commit/881bcf3894f5d25ba67a533e23b907e05cd70d15))
* **tray:** remove Settings menu item (duplicate of Open ncRS) ([f7277d0](https://github.com/rgon/ncrsDesktop/commit/f7277d066e7d51ed8abd9476861286c5749014fa))
* **typecheck:** resolve all svelte-check errors ([e80c94c](https://github.com/rgon/ncrsDesktop/commit/e80c94cccc630d09167db14b192c51c2cf20f485))
* **ui:** expandable error cards, pointerdown close, ENOSPC retry storm ([9267a5f](https://github.com/rgon/ncrsDesktop/commit/9267a5f4daf3535f1b2b0802e0e45321c53c116c))
* **ui:** fix server label clipping and avatar dropdown overflow ([43cd4c5](https://github.com/rgon/ncrsDesktop/commit/43cd4c5f8555da7093b8e64c7bb0eae780503550))
* **ui:** move [@const](https://github.com/const) tags to be direct children of {#if} block ([4efb8ec](https://github.com/rgon/ncrsDesktop/commit/4efb8ecbff64f133c3f64d6a2465d3794d523123))
* **ui:** show FUSE error message in sync label instead of invisible alert span ([bd973bc](https://github.com/rgon/ncrsDesktop/commit/bd973bc2ee68afbe717b9c62fea4fb7116ffd455))
* unmount FUSE on quit, handle existing mount point, and sync tray state on all transitions ([4a853e3](https://github.com/rgon/ncrsDesktop/commit/4a853e3109d4c6ab111546b11ee1d28d981472e4))
* update mount_ncfs call signature and harden MKCOL/DELETE ops ([d745ae9](https://github.com/rgon/ncrsDesktop/commit/d745ae9b2e9de00cc9dc2e16776d02f4b928c839))
* use download arrow emblem for partial-download folders ([b93a90d](https://github.com/rgon/ncrsDesktop/commit/b93a90d5ff67bdd0fbc870b2978ee3b2033558a1))
* use Nautilus.FileInfo.lookup for invalidation instead of storing stale GObject refs ([85b4aef](https://github.com/rgon/ncrsDesktop/commit/85b4aef908436acf8629342f6fdad067dedbec1f))
* use shared buffer with condvar for incremental read-ahead streaming ([1ac65bc](https://github.com/rgon/ncrsDesktop/commit/1ac65bccec0f3d020083f36f0899a98cec44096e))
* use sync update_file_info with background cache for non-blocking NC columns ([d2efdd3](https://github.com/rgon/ncrsDesktop/commit/d2efdd3ef4ad2b208d3613b037c870365c42fa24))
* use synchronous DETAIL IPC in update_file_info for immediate NC columns ([12860a0](https://github.com/rgon/ncrsDesktop/commit/12860a04086ffc9aa53ee397410e96ff0095506a))
* wait for PROPFIND completion instead of 2s timeout, increase IPC socket timeout ([96b4fa7](https://github.com/rgon/ncrsDesktop/commit/96b4fa7f245e8631fed8bde6910628e2ec9333d7))
* wrap all Nautilus extension callbacks in try/except to prevent crashes ([fa678b8](https://github.com/rgon/ncrsDesktop/commit/fa678b8fa2af962322ec9778cc84ca1a95bd6ea5))


### Performance Improvements

* add 10s in-memory directory listing cache ([c4dd94c](https://github.com/rgon/ncrsDesktop/commit/c4dd94cb0365885ef219670f818637da9d29f1e9))
* add prefetch_throttle so aggressive_prefetch doesn't compete with READDIR ([213a34d](https://github.com/rgon/ncrsDesktop/commit/213a34dffdb9c9616e2e38d52ba890f704ec3cb2))
* add throughput metrics to range read logging ([82bb0dd](https://github.com/rgon/ncrsDesktop/commit/82bb0dd7ebf43425eb4613b129c31acb40b68fcc))
* **build:** merge two cargo build passes into one to avoid recompiling shared deps ([51b93b2](https://github.com/rgon/ncrsDesktop/commit/51b93b2ac62b536f5fe860e19694415571e0a1df))
* cap thumbnail and subdirectory prefetching for directories &gt;200 entries ([01f2a33](https://github.com/rgon/ncrsDesktop/commit/01f2a33c9a3e6964a49cb10a75ee4e9a89fa264b))
* chain prefetch one level deeper to eliminate pause between traversal waves ([85061be](https://github.com/rgon/ncrsDesktop/commit/85061be28a5ff20279f4a633bcc46f29a119540e))
* fix condvar wake + suppress proactive_refresh during traversal ([b153765](https://github.com/rgon/ncrsDesktop/commit/b1537658e0460a7bc2c8ea585b023ac3e3f073d0))
* **fuse:** deduplicate concurrent thumbnail-prefetch threads per directory ([861178d](https://github.com/rgon/ncrsDesktop/commit/861178d061c8e324f42488e0bca599a80f400bf6))
* **fuse:** defer O(n) map retain() calls to after reply.ok() in readdir ([a85c2dd](https://github.com/rgon/ncrsDesktop/commit/a85c2dd7bd4c134c429d629e4283ec75c83d499a))
* **fuse:** release cache lock before reply.add() loop to unblock concurrent getattr ([fdb907a](https://github.com/rgon/ncrsDesktop/commit/fdb907aa2ff00dce64b3eeac39ddc7edeb683c70))
* **fuse:** short-circuit second metadata() stat when kept_path already matches ([3769185](https://github.com/rgon/ncrsDesktop/commit/37691851b3affb90558a3e2a4e10422d50980f18))
* increase read-ahead to 64MB and add background prefetching ([7869377](https://github.com/rgon/ncrsDesktop/commit/7869377f63da96ec83426278416761a48c757ef5))
* increase thumb prefetch concurrency (batch 32, cap 200) and fix stray lock unwrap ([f58528e](https://github.com/rgon/ncrsDesktop/commit/f58528e5939f0f56102837a695496dfc75e7669a))
* **ipc:** aggregate DETAILDIR directory statuses in one pass instead of O(subdirs*N) scans ([0b7fe71](https://github.com/rgon/ncrsDesktop/commit/0b7fe71f5ea3f6075a04d5bd1c33fb81342e1bce))
* **ipc:** demote DETAIL log to debug, fix thumbnailer crash on malformed JPEG ([a0c247a](https://github.com/rgon/ncrsDesktop/commit/a0c247aad1ba26fc008c4b26f06ac750e32af86b))
* **ipc:** drop status/detail locks before joining DETAILDIR reply so huge dirs don't stall FUSE ([7632fae](https://github.com/rgon/ncrsDesktop/commit/7632fae39a72a552de5278c3b21da6a8c4ece92a))
* **ipc:** pre-populate detail/shared/fileid maps before readdir reply.ok() to fix race with Nautilus extension DETAIL queries ([d246cf8](https://github.com/rgon/ncrsDesktop/commit/d246cf85c9b6be8b136dc478e4bebfaa0d6cd1fa))
* make lookup/getattr cache-only, no PROPFIND triggered by Nautilus file scanning ([9ac60cf](https://github.com/rgon/ncrsDesktop/commit/9ac60cf1761eb931a575fc9ca1ff574457f01bf1))
* **nautilus:** avoid FUSE utimes upcall for M-type FILE_CHANGES events ([396829b](https://github.com/rgon/ncrsDesktop/commit/396829b0249342a85a27e2cec43dcf44057fbf82))
* **nautilus:** bound the directory metadata cache and precompute the mount prefix ([d44e126](https://github.com/rgon/ncrsDesktop/commit/d44e1266619e3219139a22f277690a0f3046ffa2))
* **nautilus:** guard poll loop against backpressure when daemon is slow ([30c32c0](https://github.com/rgon/ncrsDesktop/commit/30c32c044bfbff679bfd87f6f142d8e53ed7bcc5))
* **nautilus:** make update_file_info async via update_file_info_full + IN_PROGRESS ([bd1eaa8](https://github.com/rgon/ncrsDesktop/commit/bd1eaa82771e431758a4c4665b96b69b8ae528ca))
* **nautilus:** patch changed cache entries in place instead of refetching the whole directory on notify_push updates ([049e167](https://github.com/rgon/ncrsDesktop/commit/049e167b3fb69e0d175165bd38af8793749c31f1))
* **nautilus:** remove per-file STATUS queries from get_file_items GTK thread ([2dcc8fb](https://github.com/rgon/ncrsDesktop/commit/2dcc8fb21869a6411c5f129d4e17544fae590a02))
* **nautilus:** repaint only requested children after a fetch to avoid O(dir) invalidation on the main thread ([608625c](https://github.com/rgon/ncrsDesktop/commit/608625c9cce20321a1250d256ef075023045375f))
* **nautilus:** replace blocking _poll_keep_done loop with GLib timeout ([4a937c0](https://github.com/rgon/ncrsDesktop/commit/4a937c0821b9182e3981bfe38c438f56695ffa9d))
* **nautilus:** serve file info synchronously from a per-directory cache to fix slow listings and handle=(nil) spam ([e11853d](https://github.com/rgon/ncrsDesktop/commit/e11853d8f00c5d138eed1561a874dcef424d903c))
* non-blocking FUSE handlers with per-call WebDAV timeout ([4df097d](https://github.com/rgon/ncrsDesktop/commit/4df097da1a93c25105ccd63b6d3a187bfe28f06b))
* populate IPC maps only once per directory and cap CHANGES to 500 paths ([19147fd](https://github.com/rgon/ncrsDesktop/commit/19147fda83cf627c5e73701327dd2a2e5a64b908))
* prefetch child dirs concurrently on readdir using pending_dirs ([648b591](https://github.com/rgon/ncrsDesktop/commit/648b591720aebf2eb32a5d41963ede2411f0bdcc))
* **preview:** write thumbnail via tmp+rename to prevent concurrent corruption ([8e5227d](https://github.com/rgon/ncrsDesktop/commit/8e5227d7784743e8e029299f232670054268462a))
* reduce prefetch contention and thumbnail size for faster directory loads ([2034acc](https://github.com/rgon/ncrsDesktop/commit/2034acc0be6de714e9242191b3a2021a4e745e40))
* reduce thumbnail batch size and gate on active streams ([f8c10e2](https://github.com/rgon/ncrsDesktop/commit/f8c10e27a704a342068771a1589dfea183aa1e24))
* separate read throttle, increase read pool to 8, enable tcp_nodelay ([d430848](https://github.com/rgon/ncrsDesktop/commit/d43084829dfeb3b21f73cfc3cf9589938e1b95db))
* share HTTP client across requests and parallelize thumbnail prefetch ([e8d1fb0](https://github.com/rgon/ncrsDesktop/commit/e8d1fb0865119b5649d9a5e5303714a9c212ba90))
* short-circuit readdir continuation pages from cache ([78395d7](https://github.com/rgon/ncrsDesktop/commit/78395d783f504395f60881fb1e98b5d105f4c5eb))
* skip read throttle for small reads without read-ahead ([b9e2afb](https://github.com/rgon/ncrsDesktop/commit/b9e2afbf43e4e50e8a0c091643cec5e9cd667a80))
* split IPC population into per-map short locks to reduce FUSE contention ([62e00b4](https://github.com/rgon/ncrsDesktop/commit/62e00b401fa318b1723f4cb46d84f532b96532f2))
* stop flooding dirty set on notify_push file change events ([5b50437](https://github.com/rgon/ncrsDesktop/commit/5b504370b144935b8438e42b5adbade2bbfa1eb6))
* stream range reads to reply with first bytes immediately ([f57c34c](https://github.com/rgon/ncrsDesktop/commit/f57c34c256c2051a928e37e25b10da0295f12f39))
* stream XML parsing directly from HTTP response instead of buffering ([c79644c](https://github.com/rgon/ncrsDesktop/commit/c79644cd2b4986832404e73268f0197709c95fa7))
* switch back to async update_file_info_full with 32-worker pool for parallel DETAIL queries ([b988fec](https://github.com/rgon/ncrsDesktop/commit/b988fece1b1300dd606d53036c465c655b167ba3))
* switch Nautilus DETAIL queries to synchronous IPC, eliminate thread pool bottleneck ([f1f8691](https://github.com/rgon/ncrsDesktop/commit/f1f86912217ba142e149ba76c84878aa7ac80f5d))
* **thumbnailer,extension:** fix hang risks, spurious NC ops, and idle poll overhead ([836cae3](https://github.com/rgon/ncrsDesktop/commit/836cae308e7d781ff41240d37ae1ec3b002bbe1b))
* **upload:** stream PUT from staging file instead of loading into RAM ([07dc81a](https://github.com/rgon/ncrsDesktop/commit/07dc81a47b8ec5b4369fd0610e6e017e9727362d))
* use Arc&lt;Vec&lt;DavEntry&gt;&gt; in dir cache to eliminate O(N) clones per FUSE call ([9796f99](https://github.com/rgon/ncrsDesktop/commit/9796f99f495909c475ae3bed99289ea29c4b10ec))
* WebDAV connection pool and speculative subdirectory prefetching ([adadcc6](https://github.com/rgon/ncrsDesktop/commit/adadcc68e06eae2ab55219feb818d252b142c050))

## [0.1.17](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.16...ncrs-v0.1.17) (2026-07-12)


### Bug Fixes

* **preview:** convert NC JPEG preview response to PNG for XDG thumbnail cache ([f19f259](https://github.com/rgon/ncrsDesktop/commit/f19f259))
* **preview:** only use fileId-based NC preview API; drop unreliable path-based fallback ([f19f259](https://github.com/rgon/ncrsDesktop/commit/f19f259))

## [0.1.16](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.15...ncrs-v0.1.16) (2026-07-12)


### Bug Fixes

* **preview:** evict XDG fail-cache entries when daemon successfully writes a thumbnail ([c662680](https://github.com/rgon/ncrsDesktop/commit/c662680))

## [0.1.15](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.14...ncrs-v0.1.15) (2026-07-12)


### Bug Fixes

* **preview:** percent-encode file: URIs so XDG thumbnail hashes match Nautilus ([ed1a42d](https://github.com/rgon/ncrsDesktop/commit/ed1a42d796349797052ff15d28f37320fa198c14))

## [0.1.14](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.13...ncrs-v0.1.14) (2026-07-12)


### Performance Improvements

* **ipc:** drop status/detail locks before joining DETAILDIR reply so huge dirs don't stall FUSE ([7632fae](https://github.com/rgon/ncrsDesktop/commit/7632fae39a72a552de5278c3b21da6a8c4ece92a))
* **nautilus:** patch changed cache entries in place instead of refetching the whole directory on notify_push updates ([049e167](https://github.com/rgon/ncrsDesktop/commit/049e167b3fb69e0d175165bd38af8793749c31f1))
* **nautilus:** repaint only requested children after a fetch to avoid O(dir) invalidation on the main thread ([608625c](https://github.com/rgon/ncrsDesktop/commit/608625c9cce20321a1250d256ef075023045375f))

## [0.1.13](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.12...ncrs-v0.1.13) (2026-07-11)


### Bug Fixes

* **build:** sync Cargo.lock to workspace version 0.1.12 ([6ac52f7](https://github.com/rgon/ncrsDesktop/commit/6ac52f7590988baf7f4da5b18daeb1a5a0c098eb))
* **fuse:** update inode map on rename so saved files don't vanish ([233b5db](https://github.com/rgon/ncrsDesktop/commit/233b5dbf63c62a4faacee613d01247762ef90113))

## [0.1.12](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.11...ncrs-v0.1.12) (2026-07-11)


### Features

* **ipc:** add DETAILDIR batch command for directory metadata ([f15d22d](https://github.com/rgon/ncrsDesktop/commit/f15d22d46fb7720265b40237d3c56ea5f5c8dce8))
* **ipc:** add DETAILDIR to batch a directory's child metadata into one reply ([e1eb509](https://github.com/rgon/ncrsDesktop/commit/e1eb509cc744703b1471c691bdb4a5a99462d90c))
* **ipc:** add VERSION handshake so daemon/extension protocol mismatch is logged ([8e13f37](https://github.com/rgon/ncrsDesktop/commit/8e13f37b93bd19725eb183b8bab709d9da1050cf))
* **issues:** add Clear all button for conflicts in the warnings tab ([4a49942](https://github.com/rgon/ncrsDesktop/commit/4a499429f66362f0da9ec77f3cfca26b44166f3e))


### Bug Fixes

* **nautilus:** clamp _poll_skip to 0 to prevent negative value if pool resets it mid-decrement ([38c80aa](https://github.com/rgon/ncrsDesktop/commit/38c80aad3e40e8abbafdc767380d64e8f75cf15f))
* **nautilus:** refuse dev install when the packaged extension copy exists to avoid GObject type collision ([b3ae4f9](https://github.com/rgon/ncrsDesktop/commit/b3ae4f9a5caf76d299e16fbc4b2dc2246b4ea494))
* **thumbnailer:** route by is_remote instead of is_local so slow/absent daemon falls through to exiftool ([881bcf3](https://github.com/rgon/ncrsDesktop/commit/881bcf3894f5d25ba67a533e23b907e05cd70d15))


### Performance Improvements

* **ipc:** aggregate DETAILDIR directory statuses in one pass instead of O(subdirs*N) scans ([0b7fe71](https://github.com/rgon/ncrsDesktop/commit/0b7fe71f5ea3f6075a04d5bd1c33fb81342e1bce))
* **nautilus:** bound the directory metadata cache and precompute the mount prefix ([d44e126](https://github.com/rgon/ncrsDesktop/commit/d44e1266619e3219139a22f277690a0f3046ffa2))
* **nautilus:** serve file info synchronously from a per-directory cache to fix slow listings and handle=(nil) spam ([e11853d](https://github.com/rgon/ncrsDesktop/commit/e11853d8f00c5d138eed1561a874dcef424d903c))
* **upload:** stream PUT from staging file instead of loading into RAM ([07dc81a](https://github.com/rgon/ncrsDesktop/commit/07dc81a47b8ec5b4369fd0610e6e017e9727362d))

## [0.1.11](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.10...ncrs-v0.1.11) (2026-07-11)


### Bug Fixes

* **core:** remove erroneous MKCOL 409 idempotent arm that silently dropped journal entries ([16d81e7](https://github.com/rgon/ncrsDesktop/commit/16d81e7438ba65c92462755becf5dae0ef5ddf02))
* **nautilus:** log malformed FILE_CHANGES entries; set _poll_skip before clearing _poll_running ([e0f1562](https://github.com/rgon/ncrsDesktop/commit/e0f1562890e2e4d1cca2efb46d29ecf287f57a17))
* **thumbnailer:** catch OSError from missing exiftool; use or-fallback for XDG_RUNTIME_DIR ([7c42d8d](https://github.com/rgon/ncrsDesktop/commit/7c42d8de732540d7dcc9df8cec9d9f12cf354ee9))

## [0.1.10](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.9...ncrs-v0.1.10) (2026-07-10)


### Bug Fixes

* **cache:** delete orphaned write_* staging files after upload completes ([af1686b](https://github.com/rgon/ncrsDesktop/commit/af1686b3b4dcc2786c8bfeff1c59d51870b6e971))
* **core:** parallelize boot validation and update detail maps on background dir refresh ([553b5fe](https://github.com/rgon/ncrsDesktop/commit/553b5fea7f0481eed6128048fa0a6097c4f16e88))
* **issues:** override DaisyUI grid on alert cards, add dismiss × button ([b1a260f](https://github.com/rgon/ncrsDesktop/commit/b1a260f295e78db17a96f8e49e2fb4e275fcc9c4))
* **issues:** replace DaisyUI btn with plain icon-button classes to fix overflow ([9671b18](https://github.com/rgon/ncrsDesktop/commit/9671b182b1f3951d0544d4c891e64d2b216f0aa7))
* **settings:** add gap between toggle label and checkbox ([a28ad2e](https://github.com/rgon/ncrsDesktop/commit/a28ad2eff626f0b0fa33a8c4418092f9832e233f))


### Performance Improvements

* **ipc:** demote DETAIL log to debug, fix thumbnailer crash on malformed JPEG ([a0c247a](https://github.com/rgon/ncrsDesktop/commit/a0c247aad1ba26fc008c4b26f06ac750e32af86b))
* **ipc:** pre-populate detail/shared/fileid maps before readdir reply.ok() to fix race with Nautilus extension DETAIL queries ([d246cf8](https://github.com/rgon/ncrsDesktop/commit/d246cf85c9b6be8b136dc478e4bebfaa0d6cd1fa))
* **thumbnailer,extension:** fix hang risks, spurious NC ops, and idle poll overhead ([836cae3](https://github.com/rgon/ncrsDesktop/commit/836cae308e7d781ff41240d37ae1ec3b002bbe1b))

## [0.1.9](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.8...ncrs-v0.1.9) (2026-07-10)


### Features

* **issues:** add per-item error dismiss button ([fe5fde9](https://github.com/rgon/ncrsDesktop/commit/fe5fde94a559e80593219c983610c65e6795c343))
* replace stub 'Add account' with working Log out button ([c16bf42](https://github.com/rgon/ncrsDesktop/commit/c16bf42bab588e16e099f484e389e65d686a9548))
* **tauri:** add dismiss_error command to remove single error by timestamp ([4e71616](https://github.com/rgon/ncrsDesktop/commit/4e7161609e16ec46dda2ca4cbcfe27b0340ab9a9))


### Bug Fixes

* **ui:** expandable error cards, pointerdown close, ENOSPC retry storm ([9267a5f](https://github.com/rgon/ncrsDesktop/commit/9267a5f4daf3535f1b2b0802e0e45321c53c116c))

## [0.1.8](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.7...ncrs-v0.1.8) (2026-07-10)


### Features

* **http:** probe HTTP/3 at startup, fall back to HTTP/2 if QUIC unavailable ([8adc9b2](https://github.com/rgon/ncrsDesktop/commit/8adc9b22765a3b51f5449f33458c333b57ac5b1e))
* **settings:** add GNOME GIO intermediate auto-cleanup toggle ([7fa2692](https://github.com/rgon/ncrsDesktop/commit/7fa269218da1f2c84aa6692e0cd21d165dd2e645))


### Bug Fixes

* **core:** surface PROPFIND auth/network errors to readdir and GUI error log ([1c06516](https://github.com/rgon/ncrsDesktop/commit/1c065164e60df6aafa3b8b857958114814255b7b))
* **http:** correct misleading http3 alt-svc comment and hint ([dfd851c](https://github.com/rgon/ncrsDesktop/commit/dfd851c8edf2558d22f954859c33d688497ed2b5))
* **http:** remove http3_prior_knowledge; use alt-svc negotiation instead ([239809c](https://github.com/rgon/ncrsDesktop/commit/239809ccbe2a97b1fd92fb6a48237ad2a62c441b))

## [0.1.7](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.6...ncrs-v0.1.7) (2026-07-10)


### Features

* **config:** enable http3 by default; add explanatory comments to advanced settings ([4b34dbc](https://github.com/rgon/ncrsDesktop/commit/4b34dbc3a33bfefb601c0313ea63d4501648848a))
* **fuse:** auto-delete stale GIO atomic-write temps from server on readdir ([e02bbfd](https://github.com/rgon/ncrsDesktop/commit/e02bbfd80fd5a2a05e5c75ccef51604a002b6e3e))
* **gui:** add description hints to advanced settings toggles ([a129dcb](https://github.com/rgon/ncrsDesktop/commit/a129dcbc4b447653e11b97f4919e37dd0d860ffc))
* **gui:** add Remount button to settings footer ([3838009](https://github.com/rgon/ncrsDesktop/commit/38380091e36837c784b5a39bdf6ac86cc73b9741))
* **thumbnailer:** add CR3/CR2 raw thumbnail support via embedded JPEG preview ([0c39c1a](https://github.com/rgon/ncrsDesktop/commit/0c39c1ad47f2c45fa5127d2894a2184fd8129c5c))
* **thumbnailer:** fetch NC preview via IPC instead of reading local raw file, expand to all registered RAW MIME types ([b5d44c4](https://github.com/rgon/ncrsDesktop/commit/b5d44c48cedc9394d9c4cfe22ce56a1975406de2))


### Bug Fixes

* **fuse:** delete all GIO temps on PROPFIND, not just age-threshold ones ([921d2a7](https://github.com/rgon/ncrsDesktop/commit/921d2a77d076aede28f65c00b91fdfaf21ab86a1))

## [0.1.6](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.5...ncrs-v0.1.6) (2026-07-10)


### Bug Fixes

* **preview:** throttle on-demand RAW thumbnail fetches to avoid starving FUSE HTTP workers ([8473f2b](https://github.com/rgon/ncrsDesktop/commit/8473f2b5561834a7d88d7f690278ac8d36e345a4))


### Performance Improvements

* **fuse:** deduplicate concurrent thumbnail-prefetch threads per directory ([861178d](https://github.com/rgon/ncrsDesktop/commit/861178d061c8e324f42488e0bca599a80f400bf6))
* **fuse:** defer O(n) map retain() calls to after reply.ok() in readdir ([a85c2dd](https://github.com/rgon/ncrsDesktop/commit/a85c2dd7bd4c134c429d629e4283ec75c83d499a))
* **fuse:** release cache lock before reply.add() loop to unblock concurrent getattr ([fdb907a](https://github.com/rgon/ncrsDesktop/commit/fdb907aa2ff00dce64b3eeac39ddc7edeb683c70))
* **fuse:** short-circuit second metadata() stat when kept_path already matches ([3769185](https://github.com/rgon/ncrsDesktop/commit/37691851b3affb90558a3e2a4e10422d50980f18))
* **nautilus:** avoid FUSE utimes upcall for M-type FILE_CHANGES events ([396829b](https://github.com/rgon/ncrsDesktop/commit/396829b0249342a85a27e2cec43dcf44057fbf82))
* **nautilus:** guard poll loop against backpressure when daemon is slow ([30c32c0](https://github.com/rgon/ncrsDesktop/commit/30c32c044bfbff679bfd87f6f142d8e53ed7bcc5))
* **nautilus:** make update_file_info async via update_file_info_full + IN_PROGRESS ([bd1eaa8](https://github.com/rgon/ncrsDesktop/commit/bd1eaa82771e431758a4c4665b96b69b8ae528ca))
* **nautilus:** remove per-file STATUS queries from get_file_items GTK thread ([2dcc8fb](https://github.com/rgon/ncrsDesktop/commit/2dcc8fb21869a6411c5f129d4e17544fae590a02))
* **nautilus:** replace blocking _poll_keep_done loop with GLib timeout ([4a937c0](https://github.com/rgon/ncrsDesktop/commit/4a937c0821b9182e3981bfe38c438f56695ffa9d))
* **preview:** write thumbnail via tmp+rename to prevent concurrent corruption ([8e5227d](https://github.com/rgon/ncrsDesktop/commit/8e5227d7784743e8e029299f232670054268462a))

## [0.1.5](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.4...ncrs-v0.1.5) (2026-07-10)


### Features

* **preview:** fetch thumbnails for RAW camera formats regardless of has_preview flag ([98a92cf](https://github.com/rgon/ncrsDesktop/commit/98a92cf03e6bcd3b0c5853d8a92d2bf189bf5e1c))

## [0.1.4](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.3...ncrs-v0.1.4) (2026-07-09)


### Features

* **gui:** add settings view with yaml config editing and version indicator ([63b3154](https://github.com/rgon/ncrsDesktop/commit/63b315440767e394c001b970a35a8c121296188c))


### Bug Fixes

* **fuse:** guard newly created files in uploading set to prevent ENOENT race ([9462955](https://github.com/rgon/ncrsDesktop/commit/94629552fc2c6551995b9edfca4a413cd595ed9b))

## [0.1.3](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.2...ncrs-v0.1.3) (2026-07-09)


### Features

* **gui:** left-click tray icon opens window, right-click shows menu ([09d9295](https://github.com/rgon/ncrsDesktop/commit/09d92952867910e1d16d69c87ebc33a6887fbf64))


### Bug Fixes

* **fuse:** preserve in-flight uploads during concurrent dir cache PROPFIND refresh ([c41f68e](https://github.com/rgon/ncrsDesktop/commit/c41f68eae36ca25a0588f2585ce6893f084c650e))
* **gui:** prevent floating panel from shrinking on HiDPI-scaled displays ([80161ed](https://github.com/rgon/ncrsDesktop/commit/80161ed571ae6cb57b4d9ad4f13768d00d44ba49))


### Performance Improvements

* **build:** merge two cargo build passes into one to avoid recompiling shared deps ([51b93b2](https://github.com/rgon/ncrsDesktop/commit/51b93b2ac62b536f5fe860e19694415571e0a1df))

## [0.1.2](https://github.com/rgon/ncrsDesktop/compare/ncrs-v0.1.1...ncrs-v0.1.2) (2026-07-07)


### Features

* --auto-keep-locally-modified-files flag controls post-upload emblem and local copy ([ef4fba1](https://github.com/rgon/ncrsDesktop/commit/ef4fba1b123b0ad3faa9daaec27ced24a0c56936))
* add 'View in Nextcloud Web' right-click menu via WEBURL IPC command ([12b64cf](https://github.com/rgon/ncrsDesktop/commit/12b64cfd0b9af16a25a196510adcaced66552d5b))
* add bearer_token and auth_command to config ([9cbbc23](https://github.com/rgon/ncrsDesktop/commit/9cbbc2394b0cbf3c92a9d8075158853141513c22))
* add cache cleanup with configurable max size and auto-purge by age ([07b3e8e](https://github.com/rgon/ncrsDesktop/commit/07b3e8e62b6ae4e923abc279a8875f9d7044b503))
* add CHANGES invalidation tracing to Nautilus extension ([69bfa2f](https://github.com/rgon/ncrsDesktop/commit/69bfa2f2fe7614b16fdd244e2aa22c598dba242d))
* add CHANGES IPC command for live emblem refresh after Keep Locally ([1ea8f11](https://github.com/rgon/ncrsDesktop/commit/1ea8f11deec2d9c70197135e39598829bb97c1f0))
* add CLI argument parsing with --offline, --config, and --mount-point flags ([1409a80](https://github.com/rgon/ncrsDesktop/commit/1409a800e2227112ad729f86fc9d86cea0b2b8d6))
* add configurable cache cleanup interval (default 1h) ([f0dfe4e](https://github.com/rgon/ncrsDesktop/commit/f0dfe4eb7b68d6f23561cf1a97e71f477677ca1e))
* add ConflictsView and ErrorsView components ([c3d0a36](https://github.com/rgon/ncrsDesktop/commit/c3d0a3681fc9088798e14af33049e0700c4c9ad4))
* add Credentials auth abstraction for basic and bearer token support ([61d6488](https://github.com/rgon/ncrsDesktop/commit/61d6488015b25078a34ee38d976b2237125f6dcf))
* add deb build script ([8169ea6](https://github.com/rgon/ncrsDesktop/commit/8169ea68646ba68cb717a02f721262113c402e80))
* add exclude_folders config to hide folders from FUSE mount ([78ae3cc](https://github.com/rgon/ncrsDesktop/commit/78ae3ccdd0109dc051465059907aa9d9e3e1ae95))
* add filename validation module matching Nextcloud server rules ([6bcf3d7](https://github.com/rgon/ncrsDesktop/commit/6bcf3d7cbd66a005edd5d2d07fb0710730443980))
* add frontend plugin registry and plugins view ([549cefe](https://github.com/rgon/ncrsDesktop/commit/549cefec842dbfe7e6de870345b41c4cefa7d578))
* add fuse_notify and mutation_journal modules ([07adcf2](https://github.com/rgon/ncrsDesktop/commit/07adcf2237c526cbe664dee679ae352884120315))
* add GNOME Shell search provider for server-side Nextcloud search ([29e4595](https://github.com/rgon/ncrsDesktop/commit/29e4595540f8dfe53224a961f15434b51960fbae))
* add info-level tracing across FUSE, IPC, PROPFIND and Nautilus extension ([1d77751](https://github.com/rgon/ncrsDesktop/commit/1d7775197b86b63c23a1f666e200cb7405b2ecbe))
* add keep_paths config to auto-keep remote paths on startup ([4cdf9a8](https://github.com/rgon/ncrsDesktop/commit/4cdf9a870a3fc55638592cfb61095e7a186447ae))
* add logging ([5fe7983](https://github.com/rgon/ncrsDesktop/commit/5fe79837e2248cf75d3265e77873ba18a7af0a9e))
* add Nautilus columns for sync, sharing, permissions, owner, and size ([357a3fd](https://github.com/rgon/ncrsDesktop/commit/357a3fd3cb391c550986c04d2f22285a7f72b32f))
* add nc_passwords plugin crate with API client ([3d061f3](https://github.com/rgon/ncrsDesktop/commit/3d061f38f0a2b37e232003e5dcbd274fd68963c3))
* add nc:// URI handler for Nextcloud Edit Locally support ([fa604b4](https://github.com/rgon/ncrsDesktop/commit/fa604b48ec945ad6e98b608ce520a8e3fa6dac50))
* add ncrs_plugin shared crate with plugin trait ([8a31352](https://github.com/rgon/ncrsDesktop/commit/8a31352e1bfd1a4840d204c585f043a89a431371))
* add Nextcloud Notify Push WebSocket client for real-time cache invalidation ([5ca11cf](https://github.com/rgon/ncrsDesktop/commit/5ca11cf565f39f8e5c7c48e3952391d69660a633))
* add Nextcloud OCS notifications API to ncrs_core ([e7af4e9](https://github.com/rgon/ncrsDesktop/commit/e7af4e9ea9f1544570124f43fde07920af717644))
* add Nextcloud unified search API to ncrs_core ([b538ea5](https://github.com/rgon/ncrsDesktop/commit/b538ea5b60b3097a896426722d7ee579e9092ad6))
* add oc:permissions and oc:fileid to PROPFIND properties ([d0017d2](https://github.com/rgon/ncrsDesktop/commit/d0017d25353e115bfe246aadefd7f91b4279614f))
* add offline resilience with connectivity monitor and cache-only fallback ([747b9c9](https://github.com/rgon/ncrsDesktop/commit/747b9c98416c1c948bf6c8693032a4c6dfb96547))
* add PasswordsView frontend and register in plugin UI ([07e33de](https://github.com/rgon/ncrsDesktop/commit/07e33deef1f862b1517db0b54caee575ccbe2c17))
* add plugin registry and tray integration ([06a5cf9](https://github.com/rgon/ncrsDesktop/commit/06a5cf96bfdba4ec3f17feb68401d915e80fd4b1))
* add provider type filtering to search ([d77319f](https://github.com/rgon/ncrsDesktop/commit/d77319f68c9f1ab9a64c3c1a99d3ba1bfa02d8e5))
* add resolve_fileids() WebDAV SEARCH by fileid ([a13eaa0](https://github.com/rgon/ncrsDesktop/commit/a13eaa04566741592fd35a62566218b2c6e2e67b))
* add SEARCH IPC command and Nautilus search dialog for Nextcloud ([38e6019](https://github.com/rgon/ncrsDesktop/commit/38e6019875bcdab86e2e0ee0cf1050338951a043))
* add search_nextcloud Tauri command with parallel provider queries ([3aee79a](https://github.com/rgon/ncrsDesktop/commit/3aee79ab112108f695895663a80d3f011d4c492e))
* add SearchView component with debounced NC unified search ([f41eb99](https://github.com/rgon/ncrsDesktop/commit/f41eb993c9551d21e98e241ef65a7f4eab9c9f22))
* add systemd user service and deb packaging metadata ([7b7333b](https://github.com/rgon/ncrsDesktop/commit/7b7333b05ba2d90bc27d1967f5df75e98b677377))
* add WebDAV write operations with FUSE callbacks and ETag conflict resolution ([bdc122a](https://github.com/rgon/ncrsDesktop/commit/bdc122afd63032d03321107a0c2ff11ec42d15ce))
* add window-context detection for auto-suggesting passwords based on active browser tab ([ea1c5ec](https://github.com/rgon/ncrsDesktop/commit/ea1c5ec0e13f3aba2d5622229d7121ad7d74aa4b))
* **auth:** Nextcloud Login Flow v2 — trigger when password missing ([f8293e7](https://github.com/rgon/ncrsDesktop/commit/f8293e76f4337915c3183c547e1d5ffe72b819f3))
* **auth:** system keyring for app password; warn on insecure config perms ([35e074b](https://github.com/rgon/ncrsDesktop/commit/35e074b5377e9ed53a971ba91fc542fb9795c35d))
* auto-create mountpoint if doesn't exist ([9de011b](https://github.com/rgon/ncrsDesktop/commit/9de011bff871f1a25ca7191c8493a71e0cdc9666))
* background file caching on open, keep-locally IPC+menu, fix emblems, add debug logging ([f792818](https://github.com/rgon/ncrsDesktop/commit/f79281806ebbe772fb60f5940303276bacc7413e))
* background notification polling and dismiss/get Tauri commands ([9ad4fb5](https://github.com/rgon/ncrsDesktop/commit/9ad4fb529ec9d8ea44258faf2171fe3d44b22e8c))
* cache directory ls and update with notify-push ([415a87f](https://github.com/rgon/ncrsDesktop/commit/415a87fd0ef0208c544e90eb9bbc13a12cba7e04))
* cache downloaded files to disk, invalidate by modified time ([ee9b54d](https://github.com/rgon/ncrsDesktop/commit/ee9b54d9abb9159773ac05cc7e6b352f14371a44))
* cache streaming reads to disk when full file is covered, with configurable read-ahead ([54b441d](https://github.com/rgon/ncrsDesktop/commit/54b441d9f1c707ab44d89d5c8ddce2d06dc19e5c))
* **ci:** build the deb via build-deb.sh and verify it with test-deb.sh ([6eadd86](https://github.com/rgon/ncrsDesktop/commit/6eadd86600697fbc195d7cb042509c786ca5683a))
* **core:** add --print-default-config flag and explicit ncrs bin target ([4b9dd9d](https://github.com/rgon/ncrsDesktop/commit/4b9dd9ddad8906915c21ebf4f5eb90c5694803aa))
* **core:** add STATE, PAUSE and RESUME IPC verbs for external clients ([3b0bb03](https://github.com/rgon/ncrsDesktop/commit/3b0bb034f761c1bf4e6981ec13e9872bf4065ae3))
* deferred subdirectory readdir, partial-download icons, evict command, cache recovery, and fix web URLs ([a4c2a2b](https://github.com/rgon/ncrsDesktop/commit/a4c2a2b91dfcc91a6a866ee3fe3f10da1046db5e))
* derive FUSE mode bits from Nextcloud oc:permissions flags ([32f7be3](https://github.com/rgon/ncrsDesktop/commit/32f7be37b623cd87bed5ccd34eccaca9652012a7))
* derive Serialize/Deserialize for DavEntry for cache persistence ([7f8bc65](https://github.com/rgon/ncrsDesktop/commit/7f8bc65c9ea3f1e12ea5f06c5a0dcfa55f64bb98))
* detect shared files via oc:share-types and show shared emblem in Nautilus ([22d65df](https://github.com/rgon/ncrsDesktop/commit/22d65df9bbbcb7eae2e80e4e8cee1e29538c0f64))
* dir cache persistence, sync reads, HTTP/3 client, child prefetch batching ([1ced20d](https://github.com/rgon/ncrsDesktop/commit/1ced20d4187746245546bb379d58b47a00466aaa))
* expose error_log, transfer_map, and journal through Tauri state and commands ([ab5a4e4](https://github.com/rgon/ncrsDesktop/commit/ab5a4e459c60275c033c5ee4b99fa1c2f5cd266c))
* extend SyncProgressView with transfers and error indicators ([609512d](https://github.com/rgon/ncrsDesktop/commit/609512dac488fa72c96e864e6eaaab76c8d359c6))
* ghost entries + inotify events for all FUSE ops with Nautilus DBus reload ([8003ed4](https://github.com/rgon/ncrsDesktop/commit/8003ed4e8780dfd6ca77de0003e7f33c4e5e0c15))
* **gui:** attach to a running daemon over IPC instead of mounting a second time ([0f9c804](https://github.com/rgon/ncrsDesktop/commit/0f9c804a64bfa21507af1ef52ab641d54afddc09))
* **gui:** mirror journal and conflicts from daemon in attach mode ([6e43466](https://github.com/rgon/ncrsDesktop/commit/6e43466534b63a690d65732d401b0638369af321))
* handle FUSE unmount event with shutdown flag, GUI remount ([e6238f8](https://github.com/rgon/ncrsDesktop/commit/e6238f8764f1191a9e191837b853c149971ca4ac))
* HTTP Range partial reads with 2MB read-ahead buffer for streaming ([9a10ebc](https://github.com/rgon/ncrsDesktop/commit/9a10ebcbfd424280a4b5f86bd65fc90f760f3bb1))
* implement base features in UI, match most NC Desktop components. Open in top right corner (hack for Wayland) ([68c39eb](https://github.com/rgon/ncrsDesktop/commit/68c39eb286b7358e19bd2fe4ae809c9fbfff04bb))
* implement basic configuration parser ([dc012ee](https://github.com/rgon/ncrsDesktop/commit/dc012ee6bfdc9fb6fedc90b85fa1127a78a82f2f))
* implement basic Tauri UI skeleton with tray icon and menu ([2abaeea](https://github.com/rgon/ncrsDesktop/commit/2abaeea12bb6965f4d6214f3fc6a2488e18ff5a2))
* implement chunked uploads for files larger than 10MB ([a215449](https://github.com/rgon/ncrsDesktop/commit/a2154494fa9295e796d54d318efda65103d31de3))
* implement pause sync via shared paused flag in core and GUI ([a8f3134](https://github.com/rgon/ncrsDesktop/commit/a8f31347810fe9413efc0e6abfa84118f408fa36))
* incremental readdir with channel-based streaming for cold cache ([01c3736](https://github.com/rgon/ncrsDesktop/commit/01c37366d03483fc841e6bc86e55a409adc1f32a))
* load config from ~/.config/ncrs/config.yaml, create skeleton on first run ([cb7ca65](https://github.com/rgon/ncrsDesktop/commit/cb7ca65078b030402e57197ff8215fde46b3be29))
* **login:** pre-fill server URL from existing config ([bd7d003](https://github.com/rgon/ncrsDesktop/commit/bd7d003ed4c7ac9a7ad14870dcb79561f8751c49))
* nautilus extension for file sync status emblems ([b63b021](https://github.com/rgon/ncrsDesktop/commit/b63b0217d494079b0ba14a5fffe8f926261d6573))
* open file results in file browser with view-online button ([d844d80](https://github.com/rgon/ncrsDesktop/commit/d844d8013f340d4d47f2087169330d0673b7af97))
* **packaging:** ship GUI desktop entry, icons, autostart and example config in the deb ([e399413](https://github.com/rgon/ncrsDesktop/commit/e39941328fdf3657952b469d35c9166712a6bfb5))
* populate IPC detail for directories themselves via PROPFIND self-entry ([b7b0bb8](https://github.com/rgon/ncrsDesktop/commit/b7b0bb80ee39ba4fc379cf5fd3e850a15cb9ad17))
* prefetch NC preview thumbnails into XDG cache on directory listing ([16ed616](https://github.com/rgon/ncrsDesktop/commit/16ed6168a2532018df6d89bbc681aac276b09514))
* proactively propfind invalidated etags of /* at boot ([18a9e2e](https://github.com/rgon/ncrsDesktop/commit/18a9e2eedf9e1d1a0cc62ef85bb64f257af22a49))
* raw PROPFIND with NC properties (has-preview, etag, oc:size), replace remotefs for listings ([c78fd46](https://github.com/rgon/ncrsDesktop/commit/c78fd468d0ef57aa3a0cde2423cd2a204a9da4da))
* redesign PasswordsView as quick-access popup with click-to-copy ([6a28378](https://github.com/rgon/ncrsDesktop/commit/6a283789f1e2146f429a9dafcfd67afbcf92a8d8))
* register nc_passwords plugin in tauri backend ([853a457](https://github.com/rgon/ncrsDesktop/commit/853a45791512e0b1ca16975eaaeb7a824e39063b))
* replace dummy notifications with real NC API data, avatar, and dismiss ([ebc156b](https://github.com/rgon/ncrsDesktop/commit/ebc156b11cff81e82d8ef06833a501d575df3c60))
* send OS notifications ([6f6467a](https://github.com/rgon/ncrsDesktop/commit/6f6467ace26d3befb1a11f74e95f1cb1fa949ad0))
* separate kept and cached files into distinct directories with per-file status ([9157a48](https://github.com/rgon/ncrsDesktop/commit/9157a485777ecc44fb60891608f39da04569db10))
* set x-gvfs-notrash mount option to prevent Nautilus trash dirs on Nextcloud ([6b10f5c](https://github.com/rgon/ncrsDesktop/commit/6b10f5cb83f5b231eca1e550289a43f61c86ed2f))
* show correct size and uploading emblem for newly written files ([b65f7f0](https://github.com/rgon/ncrsDesktop/commit/b65f7f0994b49d8f7922b189174b82aae027657a))
* show storage usage in GUI for kept files, cache, and server quota ([8c8b207](https://github.com/rgon/ncrsDesktop/commit/8c8b207ede8bd8c5e13c48a9e266b73ec9074c0f))
* stale-while-revalidate for directory cache, serve cached listings immediately ([96d6c20](https://github.com/rgon/ncrsDesktop/commit/96d6c2029b19fd5985b7aa85c7e7119df616e9ca))
* support Nextcloud remote wipe to delete local data on server command ([0dd7c38](https://github.com/rgon/ncrsDesktop/commit/0dd7c386937e5c6dfa28b904aeb92a0c07a9e292))
* **theme:** fetch Nextcloud server accent color from capabilities and apply to UI ([0371c1b](https://github.com/rgon/ncrsDesktop/commit/0371c1b56610140522073c8db12250be99520afc))
* thread http3 flag through notification and search clients ([f9ad7fe](https://github.com/rgon/ncrsDesktop/commit/f9ad7fe2ec9cbe8a3038f5fd544b43e68daaed62))
* **ui:** detect system dark/light mode and add design tokens ([0f244f5](https://github.com/rgon/ncrsDesktop/commit/0f244f569eb577130f6abd3bea6d13d66c321fc7))
* unix socket IPC server for file sync status queries ([1c2c86f](https://github.com/rgon/ncrsDesktop/commit/1c2c86fbb658409197ffa52d6143880a275b46ff))
* update nautilus extenision on runui ([b752dfa](https://github.com/rgon/ncrsDesktop/commit/b752dfad782d8ab477a18aadadebc7ed4f4b1545))
* upgrade reqwest 0.11 to 0.12 with HTTP/3 QUIC support ([ec801f1](https://github.com/rgon/ncrsDesktop/commit/ec801f12fee69d808ce6cb425b6b42336b6328d6))
* wire errors, transfers, and conflicts state into main page ([6e67051](https://github.com/rgon/ncrsDesktop/commit/6e67051d7bf94e2272be15a729b2e1d2919031ca))
* wire tauri commands to real backend state and config ([0ce8b89](https://github.com/rgon/ncrsDesktop/commit/0ce8b891679b71dce7312f75fec10c66b8c1ea43))


### Bug Fixes

* add done flag to stream buffer so waiters fail fast on short downloads ([e5205a0](https://github.com/rgon/ncrsDesktop/commit/e5205a029fd1153ba506e205f63e0f6abaf9508a))
* batch-populate IPC maps before readdir reply for immediate column data ([fc322f2](https://github.com/rgon/ncrsDesktop/commit/fc322f2a17405779ffda4973a8816d03363450d0))
* bound IPC connections to 64 with 60s read timeout to prevent thread leaks ([5917299](https://github.com/rgon/ncrsDesktop/commit/5917299fdb22c20cdebe5240b2904ba323006b04))
* cancel previous download when new range read starts for same fh ([7177ee4](https://github.com/rgon/ncrsDesktop/commit/7177ee4a30af799771151e6af813b7cd390a1a7f))
* check dir cache before PROPFIND in keep-locally to avoid querying file paths as directories ([87a4e68](https://github.com/rgon/ncrsDesktop/commit/87a4e689a368096ac494ac4e53d11112cf242983))
* **cicd:** proper release-please config ([9798a78](https://github.com/rgon/ncrsDesktop/commit/9798a78c30d8ed21183b734e12a809bfb193599a))
* **ci:** pass token explicitly to release-please action ([3b2ce31](https://github.com/rgon/ncrsDesktop/commit/3b2ce3199eecf0b1627815d8672befb075ee4fd2))
* clean up stale zero-byte write_* temp files on daemon startup ([cdb1599](https://github.com/rgon/ncrsDesktop/commit/cdb1599b1c2c8ad04073b8ed15731531ef73d49b))
* **core:** refuse to mount over a live mount or non-empty dir, remove mount dir on exit ([d9a6412](https://github.com/rgon/ncrsDesktop/commit/d9a6412113f1f5194b4f96d9e1a8259161d9a720))
* **core:** validate mount point before touching the IPC socket and shared state ([f196be8](https://github.com/rgon/ncrsDesktop/commit/f196be82d76580cd9f0b520f0a33d9487af817a1))
* correct Nextcloud oc:permissions flag mapping in Nautilus extension ([6685d6f](https://github.com/rgon/ncrsDesktop/commit/6685d6fa1f2745add45b7324a068a0de350333ac))
* dirty file path after PUT so Nautilus clears uploading emblem automatically ([2f39435](https://github.com/rgon/ncrsDesktop/commit/2f39435dae58178ef3a65549bca495bcba0af5e6))
* don't invalidate directories on boot if etag not changed, better atime notify-push ignore after our own propfind to prevent infinite loops ([7e5074a](https://github.com/rgon/ncrsDesktop/commit/7e5074a725ef5a1a5237d6a2885bd8dbd5444373))
* drop AutoUnmount — fuser 0.17 requires allow_other with auto_unmount ([8d2da94](https://github.com/rgon/ncrsDesktop/commit/8d2da94898881c26d4fe7cbdc0955e481ce2e511))
* enable rustls-tls for tungstenite and retry notify_push discovery on failure ([871f45e](https://github.com/rgon/ncrsDesktop/commit/871f45e898be7295540f9dab1720c76c2c9cc830))
* enforce Nextcloud oc:permissions via DefaultPermissions FUSE mount option; add perms_to_mode tests ([ced8a5c](https://github.com/rgon/ncrsDesktop/commit/ced8a5c67b9bc4b5b3a9b349847b230a7e894288))
* fetch parent directory on lookup cache miss after daemon restart ([a79b075](https://github.com/rgon/ncrsDesktop/commit/a79b0755dc50f799f6f3472fde9c5ca71f43b325))
* gate child-dir PROPFIND prefetch behind aggressive_prefetch and add HTTP request throttle ([5ec0d5b](https://github.com/rgon/ncrsDesktop/commit/5ec0d5b29e682d5a0bb592f4ff2aabd17bd4b2ea))
* green-checkmark after upload; NC properties appear via forced PROPFIND ([f42c403](https://github.com/rgon/ncrsDesktop/commit/f42c4030e517ddc2541be70165bb8a72ec560063))
* guard unlink/rmdir/rename with NC D/N/V flags; block delete in create-only shared dirs ([476a2bd](https://github.com/rgon/ncrsDesktop/commit/476a2bd7bcf728e72b772dac56e8100f1a8a1f69))
* **gui:** embed tray icons at compile time and enforce a single app instance ([1afc89d](https://github.com/rgon/ncrsDesktop/commit/1afc89d6cbc519ec435f558bd1ff9b4e29c72ab8))
* **gui:** pin plugin bare imports to local node_modules for production builds ([e49e969](https://github.com/rgon/ncrsDesktop/commit/e49e969dd98a28927cd41757463838be5d1965e1))
* **gui:** regenerate app icons from the ncrs brand mark instead of the tauri template ([c87d7ef](https://github.com/rgon/ncrsDesktop/commit/c87d7efa2aefaead42fb059314a085d2613d4089))
* **gui:** use composedPath for click-outside detection of detached nodes ([74922f5](https://github.com/rgon/ncrsDesktop/commit/74922f5d2ada957fbcc80bdff1fc04fc86d19aae))
* harden bearer auth — redact secrets, stream downloads, pre_auth WS, validate creds ([eccecbe](https://github.com/rgon/ncrsDesktop/commit/eccecbe238bc19f00006d2b41eda98c1892ad768))
* harden FUSE, IPC, and Nautilus extension against panics and errors ([40aee6e](https://github.com/rgon/ncrsDesktop/commit/40aee6e3bb78320b7830da0a71872b4587389d1a))
* lazy-unmount stale FUSE mount before remounting on restart ([bd4cec5](https://github.com/rgon/ncrsDesktop/commit/bd4cec563fc25f0e1ffba4beb431f39039084fa4))
* **login:** correct init endpoint to /index.php/login/v2 and strip WebDAV paths from user input ([4410f91](https://github.com/rgon/ncrsDesktop/commit/4410f91c3931b208fcdf161903dd1f6fbfb4c0e2))
* **login:** send User-Agent header so Nextcloud shows app name in OAuth grant page ([e7e9320](https://github.com/rgon/ncrsDesktop/commit/e7e9320e5a59bd708e31f3eba39f835bd11b216f))
* make search async with cancellation and debounce throttling ([5ac2743](https://github.com/rgon/ncrsDesktop/commit/5ac2743cf20ba0cb3e098edf43db34175366bb80))
* **mount:** remove IPC socket on FUSE teardown so remount doesn't enter attach mode ([bf5c7d5](https://github.com/rgon/ncrsDesktop/commit/bf5c7d553e5ee07fe4a10c2c4f595b37828876c7))
* **mount:** surface FUSE errors to UI and allow remount from error state ([9aa6217](https://github.com/rgon/ncrsDesktop/commit/9aa6217e0842bb4a3355effe93c56bffc9efb236))
* move DETAIL logic into update_file_info_full which Nautilus 4 actually calls ([c61c494](https://github.com/rgon/ncrsDesktop/commit/c61c49420f28251038360dccd5988657c0ad4c88))
* never return empty readdir for deferred dirs, debounce dir cache saves ([bc5bc20](https://github.com/rgon/ncrsDesktop/commit/bc5bc20ca94d0351dc71d9bd940d989c43741e32))
* only prefetch subdirs/thumbnails on first readdir, deduplicate prefetch PROPFINDs ([4f5db18](https://github.com/rgon/ncrsDesktop/commit/4f5db180af9b71d6d573fa1db9dd0363d677f5fe))
* **packaging:** build GUI with custom-protocol so the deb embeds the frontend ([3f8c706](https://github.com/rgon/ncrsDesktop/commit/3f8c706a565a70d5308bcab9d8e8ba092585a601))
* parse statusCode as string to match Nextcloud Passwords API response ([25cc8bc](https://github.com/rgon/ncrsDesktop/commit/25cc8bcdef1ce3ffa5954e5af86f9eb33e3911f2))
* percent-decode paths from remotefs-webdav list_dir results ([84d13c7](https://github.com/rgon/ncrsDesktop/commit/84d13c740af5a84361daad3da230c6972a3de69d))
* percent-decode search result titles and paths ([ff294c1](https://github.com/rgon/ncrsDesktop/commit/ff294c1406a1921236fbea9473cc9b2bbdf26542))
* populate IPC maps from getattr/lookup, serve from file_cache in read, move poll off main thread ([29ae52c](https://github.com/rgon/ncrsDesktop/commit/29ae52cde2db51d2a38fb23461741d10ca15d4a4))
* prevent concurrent PROPFIND race in get_or_list_dir ([5d1a506](https://github.com/rgon/ncrsDesktop/commit/5d1a506648dcb423548c3730557920d2e3e83bfb))
* prevent recursive LOG IPC calls, increase socket timeout and recv buffer ([cd57209](https://github.com/rgon/ncrsDesktop/commit/cd57209e5f1489bb1c677f23c34ae4a19fa57b5e))
* reduce Keep Locally concurrency to 2 with yield to avoid Nautilus freeze ([708e4f5](https://github.com/rgon/ncrsDesktop/commit/708e4f547da884bc32a9da3b4eb4539371fd471c))
* reduce thumbnail batch to 4, add 200ms inter-batch and 500ms initial delay ([766fa25](https://github.com/rgon/ncrsDesktop/commit/766fa25b50a23e520ca408d521db543c84063ca5))
* reject empty bearer_token, prevent notification poll from killing other pollers ([f18f696](https://github.com/rgon/ncrsDesktop/commit/f18f6963d45d010945f3840235724315c2138253))
* **release:** annotate workspace version for release-please and unify plugin versions ([d818b7e](https://github.com/rgon/ncrsDesktop/commit/d818b7e79ada55ba356752b2f8f38ee95d8c5a66))
* **release:** use generic extra-file updater for workspace Cargo.toml ([6d0ebb2](https://github.com/rgon/ncrsDesktop/commit/6d0ebb2ee4482e357ee041b4188b523d9c662744))
* **release:** use rust release type so release-please updates workspace Cargo.toml version ([9200368](https://github.com/rgon/ncrsDesktop/commit/920036846d857197608f4755f24b6f74ad45fb43))
* remove dbus reload, let inotify events handle per-file nautilus updates ([fde890d](https://github.com/rgon/ncrsDesktop/commit/fde890d2964964e41a7e9ffd5cdbfd3d6793f92c))
* remove update_file_info stub that blocked async update_file_info_full ([3d32df1](https://github.com/rgon/ncrsDesktop/commit/3d32df1cc9401e5249f26b0f27277331bf4a21ae))
* replace hard cancel with soft self-cancel, limit read throttle to 3 ([4ce0496](https://github.com/rgon/ncrsDesktop/commit/4ce049623652c19feb15f667c47bcd1318351861))
* resolve $plugins alias with absolute path and add plugin loading diagnostics ([a660c06](https://github.com/rgon/ncrsDesktop/commit/a660c06353ea6eb40f06db9246cbde9aa0cc3757))
* restore update_file_info stub, add DETAIL_ASYNC tracing ([ba29455](https://github.com/rgon/ncrsDesktop/commit/ba29455c7a5f68d25980b3d412e7801738bd927c))
* return COMPLETE synchronously for non-mount files to avoid Nautilus async overhead ([0389ab8](https://github.com/rgon/ncrsDesktop/commit/0389ab86025d64c2e282b3dbcafe1ef874a2793d))
* rewrite nautilus extension with non-blocking async update and tests ([178214c](https://github.com/rgon/ncrsDesktop/commit/178214cd812b124a86007868a3f9df086723b04c))
* **security:** chmod 0600 config file to protect app password ([5ca3337](https://github.com/rgon/ncrsDesktop/commit/5ca3337ed7df7cb2d897d770698f08fe9a5e370c))
* send self-entry immediately via channel, mark paths dirty after IPC population ([29c5a98](https://github.com/rgon/ncrsDesktop/commit/29c5a98482ae6766332ef2edf502d57303dbfdb4))
* skip background download for streaming media, show blue emblem during active downloads ([94452e5](https://github.com/rgon/ncrsDesktop/commit/94452e5deb7b5143cbf19c17cecc0128f49bfd9d))
* skip IPC socket queries for files outside ncrs mount point ([eefdd6d](https://github.com/rgon/ncrsDesktop/commit/eefdd6d8d6701f3e3a2b1e025cacb61794ef156d))
* skip zero-byte cached files and clean up failed downloads ([e1f8c4a](https://github.com/rgon/ncrsDesktop/commit/e1f8c4a247a41360a101fc4ac2f2bb948c6a5ff6))
* stop eager full-file download on open, use fileId for preview API ([6c60c37](https://github.com/rgon/ncrsDesktop/commit/6c60c37e92d4a146bcf685dce5d04cbdfb7af5c3))
* suppress notify_push self-notification loop via ETag pre-check ([f5350b1](https://github.com/rgon/ncrsDesktop/commit/f5350b10b9f14a4764c6ed7c5e598b033486c94e))
* suppress self-notify kernel dentry invalidation for freshly-fetched dirs ([e492b37](https://github.com/rgon/ncrsDesktop/commit/e492b37325353ff32925a7f23a449ccc41f0bc9b))
* **tray:** remove Settings menu item (duplicate of Open ncRS) ([f7277d0](https://github.com/rgon/ncrsDesktop/commit/f7277d066e7d51ed8abd9476861286c5749014fa))
* **typecheck:** resolve all svelte-check errors ([e80c94c](https://github.com/rgon/ncrsDesktop/commit/e80c94cccc630d09167db14b192c51c2cf20f485))
* **ui:** fix server label clipping and avatar dropdown overflow ([43cd4c5](https://github.com/rgon/ncrsDesktop/commit/43cd4c5f8555da7093b8e64c7bb0eae780503550))
* **ui:** move [@const](https://github.com/const) tags to be direct children of {#if} block ([4efb8ec](https://github.com/rgon/ncrsDesktop/commit/4efb8ecbff64f133c3f64d6a2465d3794d523123))
* **ui:** show FUSE error message in sync label instead of invisible alert span ([bd973bc](https://github.com/rgon/ncrsDesktop/commit/bd973bc2ee68afbe717b9c62fea4fb7116ffd455))
* unmount FUSE on quit, handle existing mount point, and sync tray state on all transitions ([4a853e3](https://github.com/rgon/ncrsDesktop/commit/4a853e3109d4c6ab111546b11ee1d28d981472e4))
* update mount_ncfs call signature and harden MKCOL/DELETE ops ([d745ae9](https://github.com/rgon/ncrsDesktop/commit/d745ae9b2e9de00cc9dc2e16776d02f4b928c839))
* use download arrow emblem for partial-download folders ([b93a90d](https://github.com/rgon/ncrsDesktop/commit/b93a90d5ff67bdd0fbc870b2978ee3b2033558a1))
* use Nautilus.FileInfo.lookup for invalidation instead of storing stale GObject refs ([85b4aef](https://github.com/rgon/ncrsDesktop/commit/85b4aef908436acf8629342f6fdad067dedbec1f))
* use shared buffer with condvar for incremental read-ahead streaming ([1ac65bc](https://github.com/rgon/ncrsDesktop/commit/1ac65bccec0f3d020083f36f0899a98cec44096e))
* use sync update_file_info with background cache for non-blocking NC columns ([d2efdd3](https://github.com/rgon/ncrsDesktop/commit/d2efdd3ef4ad2b208d3613b037c870365c42fa24))
* use synchronous DETAIL IPC in update_file_info for immediate NC columns ([12860a0](https://github.com/rgon/ncrsDesktop/commit/12860a04086ffc9aa53ee397410e96ff0095506a))
* wait for PROPFIND completion instead of 2s timeout, increase IPC socket timeout ([96b4fa7](https://github.com/rgon/ncrsDesktop/commit/96b4fa7f245e8631fed8bde6910628e2ec9333d7))
* wrap all Nautilus extension callbacks in try/except to prevent crashes ([fa678b8](https://github.com/rgon/ncrsDesktop/commit/fa678b8fa2af962322ec9778cc84ca1a95bd6ea5))


### Performance Improvements

* add 10s in-memory directory listing cache ([c4dd94c](https://github.com/rgon/ncrsDesktop/commit/c4dd94cb0365885ef219670f818637da9d29f1e9))
* add prefetch_throttle so aggressive_prefetch doesn't compete with READDIR ([213a34d](https://github.com/rgon/ncrsDesktop/commit/213a34dffdb9c9616e2e38d52ba890f704ec3cb2))
* add throughput metrics to range read logging ([82bb0dd](https://github.com/rgon/ncrsDesktop/commit/82bb0dd7ebf43425eb4613b129c31acb40b68fcc))
* cap thumbnail and subdirectory prefetching for directories &gt;200 entries ([01f2a33](https://github.com/rgon/ncrsDesktop/commit/01f2a33c9a3e6964a49cb10a75ee4e9a89fa264b))
* chain prefetch one level deeper to eliminate pause between traversal waves ([85061be](https://github.com/rgon/ncrsDesktop/commit/85061be28a5ff20279f4a633bcc46f29a119540e))
* fix condvar wake + suppress proactive_refresh during traversal ([b153765](https://github.com/rgon/ncrsDesktop/commit/b1537658e0460a7bc2c8ea585b023ac3e3f073d0))
* increase read-ahead to 64MB and add background prefetching ([7869377](https://github.com/rgon/ncrsDesktop/commit/7869377f63da96ec83426278416761a48c757ef5))
* increase thumb prefetch concurrency (batch 32, cap 200) and fix stray lock unwrap ([f58528e](https://github.com/rgon/ncrsDesktop/commit/f58528e5939f0f56102837a695496dfc75e7669a))
* make lookup/getattr cache-only, no PROPFIND triggered by Nautilus file scanning ([9ac60cf](https://github.com/rgon/ncrsDesktop/commit/9ac60cf1761eb931a575fc9ca1ff574457f01bf1))
* non-blocking FUSE handlers with per-call WebDAV timeout ([4df097d](https://github.com/rgon/ncrsDesktop/commit/4df097da1a93c25105ccd63b6d3a187bfe28f06b))
* populate IPC maps only once per directory and cap CHANGES to 500 paths ([19147fd](https://github.com/rgon/ncrsDesktop/commit/19147fda83cf627c5e73701327dd2a2e5a64b908))
* prefetch child dirs concurrently on readdir using pending_dirs ([648b591](https://github.com/rgon/ncrsDesktop/commit/648b591720aebf2eb32a5d41963ede2411f0bdcc))
* reduce prefetch contention and thumbnail size for faster directory loads ([2034acc](https://github.com/rgon/ncrsDesktop/commit/2034acc0be6de714e9242191b3a2021a4e745e40))
* reduce thumbnail batch size and gate on active streams ([f8c10e2](https://github.com/rgon/ncrsDesktop/commit/f8c10e27a704a342068771a1589dfea183aa1e24))
* separate read throttle, increase read pool to 8, enable tcp_nodelay ([d430848](https://github.com/rgon/ncrsDesktop/commit/d43084829dfeb3b21f73cfc3cf9589938e1b95db))
* share HTTP client across requests and parallelize thumbnail prefetch ([e8d1fb0](https://github.com/rgon/ncrsDesktop/commit/e8d1fb0865119b5649d9a5e5303714a9c212ba90))
* short-circuit readdir continuation pages from cache ([78395d7](https://github.com/rgon/ncrsDesktop/commit/78395d783f504395f60881fb1e98b5d105f4c5eb))
* skip read throttle for small reads without read-ahead ([b9e2afb](https://github.com/rgon/ncrsDesktop/commit/b9e2afbf43e4e50e8a0c091643cec5e9cd667a80))
* split IPC population into per-map short locks to reduce FUSE contention ([62e00b4](https://github.com/rgon/ncrsDesktop/commit/62e00b401fa318b1723f4cb46d84f532b96532f2))
* stop flooding dirty set on notify_push file change events ([5b50437](https://github.com/rgon/ncrsDesktop/commit/5b504370b144935b8438e42b5adbade2bbfa1eb6))
* stream range reads to reply with first bytes immediately ([f57c34c](https://github.com/rgon/ncrsDesktop/commit/f57c34c256c2051a928e37e25b10da0295f12f39))
* stream XML parsing directly from HTTP response instead of buffering ([c79644c](https://github.com/rgon/ncrsDesktop/commit/c79644cd2b4986832404e73268f0197709c95fa7))
* switch back to async update_file_info_full with 32-worker pool for parallel DETAIL queries ([b988fec](https://github.com/rgon/ncrsDesktop/commit/b988fece1b1300dd606d53036c465c655b167ba3))
* switch Nautilus DETAIL queries to synchronous IPC, eliminate thread pool bottleneck ([f1f8691](https://github.com/rgon/ncrsDesktop/commit/f1f86912217ba142e149ba76c84878aa7ac80f5d))
* use Arc&lt;Vec&lt;DavEntry&gt;&gt; in dir cache to eliminate O(N) clones per FUSE call ([9796f99](https://github.com/rgon/ncrsDesktop/commit/9796f99f495909c475ae3bed99289ea29c4b10ec))
* WebDAV connection pool and speculative subdirectory prefetching ([adadcc6](https://github.com/rgon/ncrsDesktop/commit/adadcc68e06eae2ab55219feb818d252b142c050))
