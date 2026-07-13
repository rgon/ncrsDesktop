# nc_gnome_integration

Registers the logged-in Nextcloud account as a GNOME Online Accounts (GOA) entry so that
GNOME Calendar, GNOME Contacts, Evolution, and any other GOA-aware app can access the
account without the user configuring it separately in Settings → Online Accounts.

## What it does

On login, ncrsDesktop calls `set_credentials(base_url, username, password)`. This plugin
then does exactly what the GOA daemon does when you add an account through the Settings UI:

1. **Writes an account entry** to `~/.config/goa-1.0/accounts.conf` using the `owncloud`
   provider (GNOME's name for Nextcloud), with CalDAV, CardDAV, and Files endpoints derived
   from the server URL. The entry is tagged `NcrsManaged=true` so the plugin can identify
   and remove it on logout without touching any manually-added accounts.

2. **Stores the app password** in GNOME Secret Service under the attribute
   `goa-identity = owncloud:gen0:<account_id>`, in the exact GVariant text format GOA's
   owncloud provider expects when it reads the credential back:
   ```
   {'password': <'the-app-password'>}
   ```

3. **Triggers a GOA reload** implicitly — the GOA daemon watches `accounts.conf` via
   inotify and picks up the new entry without any D-Bus call needed.

On logout, `clear_credentials` removes the `NcrsManaged` entry from `accounts.conf` and
deletes its Secret Service item. Manually-added accounts for the same server are never
touched.

## Enabled services

| GOA service      | Enabled | Notes                                      |
|------------------|---------|--------------------------------------------|
| CalendarEnabled  | true    | GNOME Calendar, Evolution                  |
| ContactsEnabled  | true    | GNOME Contacts, Evolution                  |
| FilesEnabled     | false   | ncrsDesktop already owns the FUSE mount    |

## Why not the GOA D-Bus API?

`org.gnome.OnlineAccounts.Manager.AddAccount` requires interactive user prompts and is
designed for the Settings UI flow. Writing directly to `accounts.conf` + Secret Service is
what GOA itself does internally and is the standard approach for headless account
provisioning on GNOME.
