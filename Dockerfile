# Build stage: static binary, no CGO, so the final image needs nothing else.
FROM golang:1.24-alpine AS build
WORKDIR /src
COPY server/ ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/crewaware .

# Run stage
FROM alpine:3.20
RUN adduser -D -u 10001 crewaware && mkdir /data && chown crewaware /data
COPY --from=build /out/crewaware /usr/local/bin/crewaware
USER crewaware
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["crewaware", "-addr", ":8080", "-data", "/data"]
