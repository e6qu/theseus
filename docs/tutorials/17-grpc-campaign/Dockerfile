FROM golang:1.24-alpine AS build
WORKDIR /src
COPY go.mod main.go ./
RUN go mod download && CGO_ENABLED=0 go build -o /health-service .

FROM scratch
COPY --from=build /health-service /health-service
ENTRYPOINT ["/health-service"]
