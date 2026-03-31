FROM golang:1.26.1-alpine AS builder
WORKDIR /app
COPY go.mod go.sum* ./
RUN go mod download 2>/dev/null || true
COPY cmd/ cmd/
RUN CGO_ENABLED=0 go build -o /aether-executor ./cmd/executor/
RUN CGO_ENABLED=0 go build -o /aether-monitor ./cmd/monitor/

FROM alpine:3.19
RUN apk add --no-cache ca-certificates
COPY --from=builder /aether-executor /usr/local/bin/
COPY --from=builder /aether-monitor /usr/local/bin/
ENTRYPOINT ["aether-executor"]
