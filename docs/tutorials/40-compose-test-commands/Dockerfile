FROM busybox:1.36
COPY writer /work/writer
RUN chmod +x /work/writer \
    && mkdir -p /state /www \
    && printf 'ok\n' > /www/health
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
