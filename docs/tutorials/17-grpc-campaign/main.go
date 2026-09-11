package main

import (
	"log"
	"net"

	"google.golang.org/grpc"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
)

func main() {
	listener, err := net.Listen("tcp", ":50051")
	if err != nil {
		log.Fatal(err)
	}
	server := grpc.NewServer()
	health := health.NewServer()
	health.SetServingStatus("tutorial.Api", healthpb.HealthCheckResponse_SERVING)
	healthpb.RegisterHealthServer(server, health)
	log.Fatal(server.Serve(listener))
}
