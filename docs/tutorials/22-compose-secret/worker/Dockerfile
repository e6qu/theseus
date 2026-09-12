FROM busybox:1.36
RUN mkdir -p /www && printf 'default image token\n' > /www/token
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
