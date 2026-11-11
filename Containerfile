# The Mneme server: a static mneme beside Alpine's Git, which ships git http-backend in
# git-daemon. See docs/decisions.md for why this base and what its digests pin.

FROM docker.io/library/rust:1.99.1-alpine@sha256:44819b27be4c97b0e4d7203bd2e02c4820f97700211b6a37fb4c246a2de59383 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --locked --release --bin mneme --target x86_64-unknown-linux-musl

FROM docker.io/library/alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f AS root
RUN apk add --no-cache --initdb --root /rootfs \
        --keys-dir /etc/apk/keys --repositories-file /etc/apk/repositories \
        alpine-release busybox git git-daemon \
    && mkdir -p /rootfs/data /rootfs/tmp \
    && chown 65532:65532 /rootfs/data \
    && chmod 1777 /rootfs/tmp

FROM scratch
COPY --from=root /rootfs /
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/mneme /usr/local/bin/mneme
ENV PATH=/usr/local/bin:/usr/bin:/bin
USER 65532:65532
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/mneme"]
CMD ["serve", "--repo", "/data/wiki.git", "--tokens", "/etc/mneme/tokens.json", "--addr", "0.0.0.0:8080"]
