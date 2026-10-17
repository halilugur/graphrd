# syntax=docker/dockerfile:1
# GraphRD server image: docker build -t graphrd .
FROM rust:1.90-slim-bookworm AS build
ARG FEATURES=""
WORKDIR /src
COPY . .
# the build directory is a BuildKit cache that outlives the layer, so a rebuild after a
# source change recompiles only what changed; the binary is copied out of it
RUN --mount=type=cache,target=/src/target,id=graphrd-target \
    --mount=type=cache,target=/usr/local/cargo/registry,id=graphrd-registry \
    cargo build --release --locked ${FEATURES:+--features $FEATURES} \
 && mkdir -p /out /empty-data && cp target/release/graphrd /out/graphrd && strip /out/graphrd

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/graphrd /usr/local/bin/graphrd
# a fresh volume takes the owner of this directory: the unprivileged user
COPY --from=build --chown=65532:65532 /empty-data /data
VOLUME /data
EXPOSE 7200
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/graphrd"]
# set GRAPHRD_AUTH=user:password to require a login
CMD ["serve", "--host", "0.0.0.0", "--port", "7200", "--data", "/data"]
