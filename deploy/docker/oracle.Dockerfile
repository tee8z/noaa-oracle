# The oracle image, assembled from the release's own binaries (see
# .github/workflows/release.yml). Nothing compiles here, so an image takes
# seconds, and with no RUN step the arm64 image needs no emulation.
#
# Build context (made by the release workflow):
#   bin/<arch>/oracle            linux binary per arch; embeds its UI
#   lib/<arch>/libduckdb.so      the DuckDB library it links against
#   share/noaa-oracle/           config examples and static/.embedded
#
# The layout matches the earlier Nix-built image: /bin/oracle,
# /lib/libduckdb.so, /share/noaa-oracle, port 9800, working directory /data.
FROM gcr.io/distroless/cc-debian13@sha256:4594d59540d1948417f6ca2829ddd9294493a7c68b7528f4dd459de7f203a750
ARG TARGETARCH
COPY bin/${TARGETARCH}/oracle /bin/oracle
COPY lib/${TARGETARCH}/libduckdb.so /lib/libduckdb.so
COPY share/noaa-oracle/ /share/noaa-oracle/
ENV LD_LIBRARY_PATH=/lib \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /data
VOLUME ["/data"]
EXPOSE 9800
CMD ["/bin/oracle"]
