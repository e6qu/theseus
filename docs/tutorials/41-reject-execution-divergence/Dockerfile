FROM busybox:1.36
RUN mkdir -p /www && printf 'ok\n' > /www/health
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
