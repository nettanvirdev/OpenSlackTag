# syntax=docker/dockerfile:1

# ── build ───────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/openslacktag /usr/local/bin/openslacktag

# ── run ─────────────────────────────────────────────────────────────────
# Node is included so MCP servers started with `npx` work out of the box.
FROM node:22-bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini tzdata \
    && rm -rf /var/lib/apt/lists/*
# The Docker CLI, for the sandboxed shell when the Docker socket is mounted.
COPY --from=docker:27-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=build /usr/local/bin/openslacktag /usr/local/bin/openslacktag
COPY --chmod=755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# Settings (.env, mcp.json, permissions.json, skills/, instructions.md) and
# data/ all live in the folder mounted here.
WORKDIR /srv
ENV DATA_DIR=/srv/data
# Starts as root only to switch to the owner of /srv; see the script.
ENTRYPOINT ["tini", "--", "docker-entrypoint.sh"]
CMD ["slack"]
