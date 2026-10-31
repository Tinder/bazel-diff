// Dummy Remote Execution API v2 server: enough for Bazel to accept it as a
// --remote_executor so --experimental_remote_require_cached engages. It never
// executes anything; every Execute call is counted and refused so the spike can
// prove nothing was dispatched.
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net"
	"os"
	"os/signal"
	"sync/atomic"
	"syscall"

	repb "github.com/bazelbuild/remote-apis/build/bazel/remote/execution/v2"
	semver "github.com/bazelbuild/remote-apis/build/bazel/semver"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type srv struct {
	repb.UnimplementedCapabilitiesServer
	repb.UnimplementedActionCacheServer
	repb.UnimplementedExecutionServer
	executes atomic.Int64
	acLookups atomic.Int64
}

func (s *srv) GetCapabilities(ctx context.Context, req *repb.GetCapabilitiesRequest) (*repb.ServerCapabilities, error) {
	return &repb.ServerCapabilities{
		CacheCapabilities: &repb.CacheCapabilities{
			DigestFunctions:               []repb.DigestFunction_Value{repb.DigestFunction_SHA256},
			ActionCacheUpdateCapabilities: &repb.ActionCacheUpdateCapabilities{UpdateEnabled: true},
		},
		ExecutionCapabilities: &repb.ExecutionCapabilities{ExecEnabled: true, DigestFunction: repb.DigestFunction_SHA256},
		LowApiVersion:         &semver.SemVer{Major: 2},
		HighApiVersion:        &semver.SemVer{Major: 2, Minor: 3},
	}, nil
}

func (s *srv) GetActionResult(ctx context.Context, req *repb.GetActionResultRequest) (*repb.ActionResult, error) {
	s.acLookups.Add(1)
	return nil, status.Error(codes.NotFound, "dummy executor holds no results")
}

func (s *srv) UpdateActionResult(ctx context.Context, req *repb.UpdateActionResultRequest) (*repb.ActionResult, error) {
	return &repb.ActionResult{}, nil
}

func (s *srv) Execute(req *repb.ExecuteRequest, stream repb.Execution_ExecuteServer) error {
	n := s.executes.Add(1)
	log.Printf("EXECUTE #%d requested (action digest %s) -- refusing", n, req.GetActionDigest().GetHash())
	return status.Error(codes.FailedPrecondition, "dummy executor never executes")
}

func main() {
	addr := flag.String("addr", "127.0.0.1:0", "listen address")
	flag.Parse()
	lis, err := net.Listen("tcp", *addr)
	if err != nil {
		log.Fatal(err)
	}
	s := &srv{}
	g := grpc.NewServer()
	repb.RegisterCapabilitiesServer(g, s)
	repb.RegisterActionCacheServer(g, s)
	repb.RegisterExecutionServer(g, s)
	fmt.Printf("grpc://%s\n", lis.Addr().String())
	os.Stdout.Sync()
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sig
		log.Printf("SUMMARY execute_calls=%d executor_ac_lookups=%d", s.executes.Load(), s.acLookups.Load())
		g.Stop()
	}()
	if err := g.Serve(lis); err != nil {
		log.Fatal(err)
	}
}
