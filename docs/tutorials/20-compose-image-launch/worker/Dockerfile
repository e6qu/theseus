FROM busybox:1.36
WORKDIR /site/default
COPY default/identity .
COPY alternate/identity /site/alternate/identity
ENTRYPOINT ["/bin/busybox", "httpd", "-f", "-p", "8080", "-h"]
CMD ["/site/default"]
