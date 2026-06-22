package main

import (
	"encoding/json"
	"io"
	"net"
	"net/http"
	"path/filepath"
	"sync"
	"testing"
)

type recordedReq struct {
	method string
	path   string
	body   map[string]any
}

// fakeFirecracker serves the API over a unix socket and records requests.
func fakeFirecracker(t *testing.T) (socket string, reqs *[]recordedReq) {
	t.Helper()
	socket = filepath.Join(t.TempDir(), "fc.sock")
	ln, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	var mu sync.Mutex
	recorded := &[]recordedReq{}
	h := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		if b, _ := io.ReadAll(r.Body); len(b) > 0 {
			_ = json.Unmarshal(b, &body)
		}
		mu.Lock()
		*recorded = append(*recorded, recordedReq{r.Method, r.URL.Path, body})
		mu.Unlock()
		w.WriteHeader(http.StatusNoContent)
	})
	srv := &http.Server{Handler: h}
	go srv.Serve(ln)
	t.Cleanup(func() { srv.Close() })
	return socket, recorded
}

func TestFCClientCalls(t *testing.T) {
	socket, reqs := fakeFirecracker(t)
	c := newFCClient(socket)

	if err := c.setMachineConfig(machineConfig{VCPUCount: 2, MemSizeMib: 4096}); err != nil {
		t.Fatal(err)
	}
	if err := c.setBootSource(bootSource{KernelImagePath: "/k", BootArgs: "x"}); err != nil {
		t.Fatal(err)
	}
	if err := c.addDrive(drive{DriveID: "rootfs", PathOnHost: "/r", IsRootDevice: true}); err != nil {
		t.Fatal(err)
	}
	if err := c.addNetworkInterface(networkInterface{IfaceID: "eth0", HostDevName: "fc-tap0", GuestMAC: "06:00:AC:10:00:02"}); err != nil {
		t.Fatal(err)
	}
	if err := c.instanceStart(); err != nil {
		t.Fatal(err)
	}
	if err := c.pause(); err != nil {
		t.Fatal(err)
	}
	if err := c.resume(); err != nil {
		t.Fatal(err)
	}
	if err := c.createSnapshot(snapshotCreate{SnapshotType: "Full", SnapshotPath: "/v", MemFilePath: "/m"}); err != nil {
		t.Fatal(err)
	}
	if err := c.loadSnapshot(snapshotLoad{SnapshotPath: "/v", MemBackend: memBackend{BackendType: "File", BackendPath: "/m"}, ResumeVM: true}); err != nil {
		t.Fatal(err)
	}

	want := []struct {
		method, path string
	}{
		{"PUT", "/machine-config"},
		{"PUT", "/boot-source"},
		{"PUT", "/drives/rootfs"},
		{"PUT", "/network-interfaces/eth0"},
		{"PUT", "/actions"},
		{"PATCH", "/vm"}, // pause
		{"PATCH", "/vm"}, // resume
		{"PUT", "/snapshot/create"},
		{"PUT", "/snapshot/load"},
	}
	if len(*reqs) != len(want) {
		t.Fatalf("want %d requests, got %d: %+v", len(want), len(*reqs), *reqs)
	}
	for i, w := range want {
		got := (*reqs)[i]
		if got.method != w.method || got.path != w.path {
			t.Fatalf("req %d: want %s %s, got %s %s", i, w.method, w.path, got.method, got.path)
		}
	}

	// Spot-check a body: drive payload should carry is_root_device.
	if v, _ := (*reqs)[2].body["is_root_device"].(bool); !v {
		t.Fatalf("addDrive body missing is_root_device: %+v", (*reqs)[2].body)
	}
	// network interface payload should carry the host TAP name.
	if (*reqs)[3].body["host_dev_name"] != "fc-tap0" {
		t.Fatalf("addNetworkInterface body wrong: %+v", (*reqs)[3].body)
	}
	// instanceStart action type.
	if (*reqs)[4].body["action_type"] != "InstanceStart" {
		t.Fatalf("instanceStart action_type wrong: %+v", (*reqs)[4].body)
	}
}

func TestFCClientErrorStatus(t *testing.T) {
	socket := filepath.Join(t.TempDir(), "fc.sock")
	ln, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"fault_message":"bad"}`)
	})}
	go srv.Serve(ln)
	t.Cleanup(func() { srv.Close() })

	c := newFCClient(socket)
	if err := c.instanceStart(); err == nil {
		t.Fatal("expected error on 400 status")
	}
}
