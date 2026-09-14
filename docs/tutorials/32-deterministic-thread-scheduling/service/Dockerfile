FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
COPY work/ledger /usr/local/bin/ledger
COPY work/ledger.theseus-schedule.json /usr/local/share/ledger.theseus-schedule.json
COPY work/ready /usr/local/bin/ready
CMD ["/usr/local/bin/ready"]
