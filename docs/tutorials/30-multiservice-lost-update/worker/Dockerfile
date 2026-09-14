FROM busybox:1.36
COPY check increment /work/
RUN chmod +x /work/check /work/increment \
    && mkdir -p /www \
    && printf 'ok\n' > /www/health
CMD ["false"]
