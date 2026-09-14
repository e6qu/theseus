FROM busybox:1.36
COPY increment /www/cgi-bin/increment
RUN chmod +x /www/cgi-bin/increment \
    && printf 'ok\n' > /www/health
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
