server {
    server_name mag.horuzprod.com;

    access_log /var/log/nginx/mag.horuzprod.com.access.log;
    error_log  /var/log/nginx/mag.horuzprod.com.error.log;

    location = /ltk-rooms {
        return 308 /ltk-rooms/;
    }

    # Rooms control plane, resumable mod transfers, WebSocket notifications, and signed updates.
    location /ltk-rooms/ {
        proxy_pass http://127.0.0.1:3000/;
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade           $http_upgrade;
        proxy_set_header Connection        "upgrade";

        client_max_body_size 5g;
        proxy_request_buffering off;
        proxy_buffering off;
        proxy_connect_timeout 15s;
        proxy_send_timeout 21600s;
        proxy_read_timeout 21600s;
        add_header X-Content-Type-Options nosniff always;
    }

    location / {
        proxy_pass http://127.0.0.1:8030;
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 60s;
    }

    listen [::]:443 ssl ipv6only=on;
    listen 443 ssl;
    ssl_certificate /etc/letsencrypt/live/mag.horuzprod.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/mag.horuzprod.com/privkey.pem;
    include /etc/letsencrypt/options-ssl-nginx.conf;
    ssl_dhparam /etc/letsencrypt/ssl-dhparams.pem;
}

server {
    if ($host = mag.horuzprod.com) {
        return 301 https://$host$request_uri;
    }

    listen 80;
    listen [::]:80;
    server_name mag.horuzprod.com;
    return 404;
}
