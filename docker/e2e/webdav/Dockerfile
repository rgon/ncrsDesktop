# Real WebDAV server for the e2e suite — Apache httpd + mod_dav. Deliberately
# NOT Nextcloud: no PHP, no OCS, no notify_push. Just standard WebDAV verbs
# (PUT/GET/DELETE/MKCOL/MOVE/PROPFIND) over HTTP Basic auth.
FROM httpd:2.4

RUN set -eux; \
    # Enable the modules mod_dav + Basic auth need (they ship commented out).
    sed -i \
      -e 's/^#\(LoadModule dav_module .*\)/\1/' \
      -e 's/^#\(LoadModule dav_fs_module .*\)/\1/' \
      -e 's/^#\(LoadModule auth_basic_module .*\)/\1/' \
      -e 's/^#\(LoadModule authn_file_module .*\)/\1/' \
      -e 's/^#\(LoadModule authn_core_module .*\)/\1/' \
      -e 's/^#\(LoadModule authz_user_module .*\)/\1/' \
      -e 's/^#\(LoadModule authz_core_module .*\)/\1/' \
      -e 's/^#\(LoadModule headers_module .*\)/\1/' \
      conf/httpd.conf; \
    echo 'Include conf/extra/ncrs-dav.conf' >> conf/httpd.conf; \
    mkdir -p /data/files /tmp/davlock; \
    # httpd child processes run as `www-data`; give it ownership of the DAV tree
    # and the lock DB so mod_dav can perform writes (PUT/MKCOL/MOVE/DELETE).
    chown -R www-data:www-data /data /tmp/davlock; \
    htpasswd -bc conf/dav.users testuser testpass

COPY ncrs-dav.conf conf/extra/ncrs-dav.conf
# Readiness is polled by the ncrs container's entrypoint, so no HEALTHCHECK here.
