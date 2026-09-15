FROM busybox:1.36
COPY test-template/ /opt/antithesis/test/v1/lost-update/
COPY smoke-template/ /opt/antithesis/test/v1/smoke/
RUN chmod +x /opt/antithesis/test/v1/lost-update/* /opt/antithesis/test/v1/smoke/* \
    && mkdir -p /state /www \
    && printf 'ok\n' > /www/health
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
