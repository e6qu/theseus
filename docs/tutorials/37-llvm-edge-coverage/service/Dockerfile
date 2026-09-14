FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
COPY work/classify /usr/local/bin/classify
COPY work/plugin.so /usr/local/lib/plugin.so
COPY work/classify.theseus-coverage.json /symbols/classify.theseus-coverage.json
COPY work/plugin.so.theseus-coverage.json /symbols/plugin.theseus-coverage.json
COPY work/symbols/ /symbols/
COPY work/ready /usr/local/bin/ready
CMD ["/usr/local/bin/ready"]
