# The daemon image, assembled from the release's own binary; see
# oracle.Dockerfile. Same layout as the earlier Nix-built image: /bin/daemon,
# working directory /data.
FROM gcr.io/distroless/cc-debian13@sha256:4594d59540d1948417f6ca2829ddd9294493a7c68b7528f4dd459de7f203a750
ARG TARGETARCH
COPY bin/${TARGETARCH}/daemon /bin/daemon
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /data
VOLUME ["/data"]
CMD ["/bin/daemon"]
