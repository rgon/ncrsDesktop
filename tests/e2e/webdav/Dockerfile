FROM nginx:1.28-alpine

RUN apk add --no-cache nginx-mod-http-dav-ext apache2-utils

RUN mkdir -p /webdav-root/remote.php/dav/files/testuser && chmod -R 777 /webdav-root
RUN mkdir -p /tmp/nginx-dav

RUN htpasswd -cb /etc/nginx/.htpasswd testuser testpass

COPY nginx.conf /etc/nginx/nginx.conf

EXPOSE 80
CMD ["nginx", "-g", "daemon off;"]
