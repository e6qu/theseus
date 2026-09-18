FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS rootfs
COPY work/ledger /rootfs/usr/local/bin/ledger
COPY work/ledger.theseus-schedule.json /rootfs/usr/local/share/ledger.theseus-schedule.json
COPY work/ready /rootfs/usr/local/bin/ready
RUN { ldd /rootfs/usr/local/bin/ledger; ldd /rootfs/usr/local/bin/ready; } \
    | awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' \
    | sort -u \
    | while read -r dependency; do cp --parents "$dependency" /rootfs; done

FROM scratch
COPY --from=rootfs /rootfs /
CMD ["/usr/local/bin/ready"]
