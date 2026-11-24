# syntax=docker/dockerfile:1.7
# Bee web app (Next.js standalone). Build from the repository root:
#   docker build -f apps/web/Dockerfile -t bee-web .

ARG BUN_VERSION=1.4.2

FROM oven/bun:${BUN_VERSION} AS builder
WORKDIR /app
COPY apps/web/package.json apps/web/bun.lock ./
RUN --mount=type=cache,target=/root/.bun/install/cache \
    bun install --frozen-lockfile
COPY apps/web/ ./
ENV NEXT_TELEMETRY_DISABLED=1
RUN bun run build

FROM node:22-alpine AS runtime
ARG BUILD_SHA=unknown
LABEL org.opencontainers.image.title="bee-web" \
      org.opencontainers.image.description="Beehive web app" \
      org.opencontainers.image.source="https://github.com/terramind-org/bee" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.revision="${BUILD_SHA}"
ENV NODE_ENV=production \
    NEXT_TELEMETRY_DISABLED=1 \
    HOSTNAME=0.0.0.0 \
    PORT=3100 \
    BEE_API_URL=http://bee-server:7420 \
    BUILD_SHA=${BUILD_SHA}
WORKDIR /app
COPY --from=builder --chown=node:node /app/.next/standalone ./
COPY --from=builder --chown=node:node /app/.next/static ./.next/static
USER node
EXPOSE 3100
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
    CMD node -e "fetch('http://127.0.0.1:'+process.env.PORT+'/').then(()=>process.exit(0),()=>process.exit(1))"
CMD ["node", "server.js"]
