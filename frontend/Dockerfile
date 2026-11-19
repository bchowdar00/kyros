#
# Kyros front end (Next.js), for Azure Container Apps.
#
# One stage: install from the committed lockfile, build, and serve with
# `next start` from the same tree. The whole install is kept (no pruning to
# production packages), which makes a larger image and leaves nothing to guess
# about what the server loads at run time.
#
# Build context is `frontend/`. Two values are inlined into the browser bundle
# at build time and must be given as build arguments:
#   NEXT_PUBLIC_API_URL    the exchange's public address (REST and /ws/*)
#   NEXT_PUBLIC_SITE_URL   this site's own public address
# Everything else is read at run time from the host: EXCHANGE_API_URL,
# CHAT_ENABLED, WRITER_MODEL, and the secrets ANTHROPIC_API_KEY and
# WRITER_METER_TOKEN. No secret is baked in.
#
# Architecture-neutral: no platform is named.
FROM node:24-bookworm-slim

# ca-certificates for outbound TLS to the exchange and to Claude.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The pnpm version pinned in package.json#packageManager.
RUN npm install --global pnpm@10.34.5

WORKDIR /app
COPY . .
RUN pnpm install --frozen-lockfile

ARG NEXT_PUBLIC_API_URL
ARG NEXT_PUBLIC_SITE_URL
ENV NEXT_PUBLIC_API_URL=$NEXT_PUBLIC_API_URL \
    NEXT_PUBLIC_SITE_URL=$NEXT_PUBLIC_SITE_URL \
    NEXT_TELEMETRY_DISABLED=1 \
    NODE_ENV=production
RUN test -n "$NEXT_PUBLIC_API_URL" && test -n "$NEXT_PUBLIC_SITE_URL" \
    && pnpm build

# The chat's conversation store is a file; it and Next's cache must be
# writable by the runtime user. node:*-slim ships a non-root `node` user.
ENV MASTRA_STORAGE_DIR=/app/.mastra-data \
    PORT=3000 \
    HOSTNAME=0.0.0.0
RUN mkdir -p /app/.mastra-data && chown -R node:node /app/.mastra-data /app/.next

USER node

EXPOSE 3000

CMD ["node_modules/.bin/next", "start"]
