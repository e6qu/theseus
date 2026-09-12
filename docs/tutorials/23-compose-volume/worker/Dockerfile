FROM busybox:1.36
RUN mkdir -p /www && printf 'default image identity\n' > /www/identity
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
