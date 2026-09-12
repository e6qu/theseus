FROM busybox:1.36
RUN mkdir -p /www && printf '{"status":"ok"}\n' > /www/health && printf 'healthy Compose worker\n' > /www/identity
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
