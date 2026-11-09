# syntax=docker/dockerfile:1
# Base images are pinned by digest; Dependabot bumps them (see .github/dependabot.yml).

FROM rust:1-slim-trixie@sha256:b8e062b1c3a80183df83d9ba187cadd2b5ab84bc78d453dbfeb2fccd5f982247 AS build
WORKDIR /app
COPY . .
# Cache mounts keep the registry and target dir between builds; the binary is copied
# out because the mounted target dir isn't part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && cp target/release/hookrelay /usr/local/bin/hookrelay
# The runtime image has no shell, so the data dir is prepared here.
RUN mkdir /data

# cc: glibc and libgcc, which the binary links against. Everything else is static.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cc66d222f0a7ec996b78a99fe76a969bc80b539a5016ad866c14d1949bc6d0ab
COPY --from=build /usr/local/bin/hookrelay /usr/local/bin/hookrelay
COPY --from=build --chown=nonroot:nonroot /data /data
VOLUME /data
ENV HOOKRELAY_ADDR=0.0.0.0:8080 \
    HOOKRELAY_DATABASE_URL=sqlite:///data/hookrelay.db
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s \
    CMD ["/usr/local/bin/hookrelay", "health"]
ENTRYPOINT ["/usr/local/bin/hookrelay"]
CMD ["serve"]
