FROM busybox:1.36
COPY test-template/ /opt/antithesis/test/v1/lost-update/
RUN chmod +x /opt/antithesis/test/v1/lost-update/* \
    && mkdir -p /state /www \
    && printf 'ok\n' > /www/health
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
