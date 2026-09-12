FROM busybox:1.36
ENV IDENTITY=worker-default
CMD ["sh", "-c", "mkdir -p /www; printf '%s\\n' \"$IDENTITY\" > /www/identity; exec httpd -f -p 8080 -h /www"]
