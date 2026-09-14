FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
COPY work/workers /usr/local/bin/workers
COPY work/workers.theseus-schedule.json /usr/local/share/workers.theseus-schedule.json
COPY work/ready /usr/local/bin/ready
CMD ["/usr/local/bin/ready"]
