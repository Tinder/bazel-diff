package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"
)

// fcClient is a minimal Firecracker REST API client speaking HTTP over the
// microVM's unix socket. Only the calls the orchestrator needs are implemented.
// See https://github.com/firecracker-microvm/firecracker/blob/main/src/firecracker/swagger/firecracker.yaml
type fcClient struct {
	http *http.Client
}

// newFCClient dials the Firecracker API unix socket at socketPath. The http
// transport ignores the URL host and always connects to that socket.
func newFCClient(socketPath string) *fcClient {
	return &fcClient{
		http: &http.Client{
			Timeout: 60 * time.Second,
			Transport: &http.Transport{
				DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
					var d net.Dialer
					return d.DialContext(ctx, "unix", socketPath)
				},
			},
		},
	}
}

func (c *fcClient) do(method, path string, body any) error {
	var rdr io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		rdr = bytes.NewReader(b)
	}
	// Host is ignored (unix socket) but must be a valid URL.
	req, err := http.NewRequest(method, "http://localhost"+path, rdr)
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	resp, err := c.http.Do(req)
	if err != nil {
		return fmt.Errorf("%s %s: %w", method, path, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 300 {
		msg, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("%s %s: status %d: %s", method, path, resp.StatusCode, string(msg))
	}
	return nil
}

// --- request payloads (subset of the Firecracker API schema) ---

type machineConfig struct {
	VCPUCount  int  `json:"vcpu_count"`
	MemSizeMib int  `json:"mem_size_mib"`
	SMT        bool `json:"smt,omitempty"`
}

type bootSource struct {
	KernelImagePath string `json:"kernel_image_path"`
	BootArgs        string `json:"boot_args,omitempty"`
}

type drive struct {
	DriveID      string `json:"drive_id"`
	PathOnHost   string `json:"path_on_host"`
	IsRootDevice bool   `json:"is_root_device"`
	IsReadOnly   bool   `json:"is_read_only"`
}

type action struct {
	ActionType string `json:"action_type"`
}

type snapshotCreate struct {
	SnapshotType string `json:"snapshot_type"` // "Full" or "Diff"
	SnapshotPath string `json:"snapshot_path"`
	MemFilePath  string `json:"mem_file_path"`
}

type snapshotLoad struct {
	SnapshotPath        string     `json:"snapshot_path"`
	MemBackend          memBackend `json:"mem_backend"`
	EnableDiffSnapshots bool       `json:"enable_diff_snapshots,omitempty"`
	ResumeVM            bool       `json:"resume_vm"`
}

type memBackend struct {
	BackendType string `json:"backend_type"` // "File" or "Uffd"
	BackendPath string `json:"backend_path"`
}

type vmState struct {
	State string `json:"state"` // "Paused" or "Resumed"
}

func (c *fcClient) setMachineConfig(cfg machineConfig) error {
	return c.do(http.MethodPut, "/machine-config", cfg)
}
func (c *fcClient) setBootSource(b bootSource) error {
	return c.do(http.MethodPut, "/boot-source", b)
}
func (c *fcClient) addDrive(d drive) error {
	return c.do(http.MethodPut, "/drives/"+d.DriveID, d)
}
func (c *fcClient) instanceStart() error {
	return c.do(http.MethodPut, "/actions", action{ActionType: "InstanceStart"})
}
func (c *fcClient) pause() error {
	return c.do(http.MethodPatch, "/vm", vmState{State: "Paused"})
}
func (c *fcClient) resume() error {
	return c.do(http.MethodPatch, "/vm", vmState{State: "Resumed"})
}
func (c *fcClient) createSnapshot(s snapshotCreate) error {
	return c.do(http.MethodPut, "/snapshot/create", s)
}
func (c *fcClient) loadSnapshot(s snapshotLoad) error {
	return c.do(http.MethodPut, "/snapshot/load", s)
}
