FROM alpine:3.22 AS build
RUN apk add --no-cache build-base
COPY probe.c /src/probe.c
RUN cc -static -O2 -Wall -Wextra -Werror -o /probe /src/probe.c

FROM busybox:1.36
COPY check increment /work/
COPY --from=build /probe /work/probe
RUN chmod +x /work/check /work/increment /work/probe \
    && mkdir -p /www \
    && printf 'ok\n' > /www/health
CMD ["false"]
