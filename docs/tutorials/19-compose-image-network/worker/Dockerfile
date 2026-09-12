FROM busybox:1.36
RUN mkdir -p /www && printf 'worker reached\n' > /www/identity
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
