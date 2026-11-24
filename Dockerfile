# copybera in a container: macOS has no cgroups, so this is how copybera runs on a Mac (and anywhere a hard memory cap
# is easier to set on a container than on a process). Build it once from this folder:
#
#   docker build -t copybera .
#
# and run every command under a hard cap (see the README's "The memory cap" and "Quickstart"):
#
#   docker run --rm --memory=1536m --memory-swap=1536m --user "$(id -u):$(id -g)" -e HOME=/tmp \
#       -v "$PWD":/work copybera validate publish/publish-subtree.toml

FROM rust:1.91.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p copybera

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/copybera /usr/local/bin/copybera
WORKDIR /work
ENTRYPOINT ["copybera"]
