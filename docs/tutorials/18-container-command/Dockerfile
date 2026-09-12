FROM busybox:1.36
RUN mkdir -p /www && printf '{"status":"ok","schema":1}\n' > /www/health
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
