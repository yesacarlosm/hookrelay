# syntax=docker/dockerfile:1
# Base images are pinned by digest; Dependabot bumps them (see .github/dependabot.yml).

FROM rust:1-slim-trixie@sha256:cc9edbcadf2c27013a642b1d1bb7504e26b1142d6b5ebe58f64cf2eb2bc128ae AS build
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
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:08476c244888bfb425bae1ae4dd7c17f093e972aab8426883d97f3c6f280f7c1
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
