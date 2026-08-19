# syntax=docker/dockerfile:1.7

# ---------------------------------------------------------------------------
# Frontend
# ---------------------------------------------------------------------------
# The output is architecture-independent, so this always builds natively on the
# builder rather than once per target platform.
FROM --platform=$BUILDPLATFORM node:22-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
# The PWA icons are committed, so the Python generator is not needed here.
RUN npm run build

# ---------------------------------------------------------------------------
# Backend
# ---------------------------------------------------------------------------
# Pinned to the builder and cross-compiled. Running the Rust toolchain under QEMU
# instead turns a two-minute build into half an hour.
FROM --platform=$BUILDPLATFORM rust:1.91-bookworm AS backend
ARG TARGETARCH
ARG BUILDARCH

# Several dependencies compile C — `mimalloc`, `bzip2-sys`, `ring` — so a cross build
# needs both a compiler and the target's libc headers. `gcc-<arch>-linux-gnu` alone
# gets as far as invoking the compiler and then fails on the first `#include`.
#
# When the target happens to match the builder, the toolchain already present is the
# right one and apt is skipped entirely.
#
# The result is written into cargo's own config rather than exported through the
# shell, so every later `cargo` invocation picks it up unchanged.
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu;  cc=x86_64-linux-gnu-gcc;  cross="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) target=aarch64-unknown-linux-gnu; cc=aarch64-linux-gnu-gcc; cross="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends $cross; \
      rm -rf /var/lib/apt/lists/*; \
    else \
      cc=cc; \
    fi; \
    rustup target add "$target"; \
    mkdir -p "$CARGO_HOME"; \
    { \
      echo '[build]'; \
      echo "target = \"$target\""; \
      echo "[target.$target]"; \
      echo "linker = \"$cc\""; \
      echo '[env]'; \
      echo "CC_$(echo "$target" | tr - _) = \"$cc\""; \
    } > "$CARGO_HOME/config.toml"; \
    echo "$target" > /etc/rust-target

WORKDIR /src
COPY backend/Cargo.toml backend/Cargo.lock ./
# A stub main so the dependency graph compiles into a layer of its own, which the
# cache can keep across source changes.
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release

COPY backend/src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eux; \
    # Without this the stub's newer timestamp can mask the real sources.
    touch src/main.rs; \
    cargo build --release --locked; \
    # The cache mount does not survive into the image, so copy the binary out.
    cp "target/$(cat /etc/rust-target)/release/regenradar-backend" /regenradar

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# TLS roots: every data source is fetched over HTTPS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home regenradar

COPY --from=backend /regenradar /usr/local/bin/regenradar
COPY --from=web /web/dist /srv/web

USER 10001
ENV REGENRADAR_BIND=0.0.0.0:8787 \
    REGENRADAR_WEB_DIR=/srv/web \
    RUST_LOG=info,tower_http=warn
EXPOSE 8787

# The binary probes itself, so the image needs no curl. Generous start period: the
# first DWD run has to be downloaded and reprojected before there is anything to serve.
HEALTHCHECK --interval=30s --timeout=5s --start-period=120s --retries=3 \
  CMD ["/usr/local/bin/regenradar", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/regenradar"]
