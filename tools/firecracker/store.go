package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// Snapshot store layout (see docs/firecracker-snapshots.md §7):
//
//	<store>/<fingerprint>/<baseSHA>/
//	  mem_file          guest memory image (diff snapshot)
//	  vmstate           Firecracker microVM state
//	  rootfs.backing    frozen read-only disk image
//	  base_hashes.json  produced by `bazel-diff warmup`
//	  metadata.json     fingerprint, baseSHA, versions, created-at

const (
	memFileName     = "mem_file"
	vmstateName     = "vmstate"
	rootfsName      = "rootfs.backing"
	baseHashesName  = "base_hashes.json"
	metadataName    = "metadata.json"
	fingerprintName = "fingerprint.json"
)

// metadata is the JSON written next to each snapshot.
type metadata struct {
	Fingerprint     string `json:"fingerprint"`
	BaseSHA         string `json:"base_sha"`
	BazelVersion    string `json:"bazel_version"`
	BazelDiffVer    string `json:"bazel_diff_version"`
	CreatedAtUnix   int64  `json:"created_at_unix"`
	CreatedAtString string `json:"created_at"`
}

// entry is a resolved store location for a (fingerprint, baseSHA) pair.
type entry struct {
	Dir     string
	BaseSHA string
}

func (e entry) memFile() string    { return filepath.Join(e.Dir, memFileName) }
func (e entry) vmstate() string    { return filepath.Join(e.Dir, vmstateName) }
func (e entry) rootfs() string     { return filepath.Join(e.Dir, rootfsName) }
func (e entry) baseHashes() string { return filepath.Join(e.Dir, baseHashesName) }
func (e entry) metadata() string   { return filepath.Join(e.Dir, metadataName) }

// entryDir returns the directory for a (fingerprint, baseSHA) snapshot.
func entryDir(store, fingerprint, baseSHA string) string {
	return filepath.Join(store, fingerprint, baseSHA)
}

// newEntry builds and creates the directory for a snapshot to be recorded.
func newEntry(store, fingerprint, baseSHA string) (entry, error) {
	dir := entryDir(store, fingerprint, baseSHA)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return entry{}, err
	}
	return entry{Dir: dir, BaseSHA: baseSHA}, nil
}

func writeMetadata(e entry, m metadata, now time.Time) error {
	m.BaseSHA = e.BaseSHA
	m.CreatedAtUnix = now.Unix()
	m.CreatedAtString = now.UTC().Format(time.RFC3339)
	b, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(e.metadata(), append(b, '\n'), 0o644)
}

// listBaseSHAs returns the base SHAs that have a complete snapshot for the
// given fingerprint. A snapshot is "complete" only if its base_hashes.json
// exists — a half-written record entry is ignored.
func listBaseSHAs(store, fingerprint string) ([]string, error) {
	dir := filepath.Join(store, fingerprint)
	ents, err := os.ReadDir(dir)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var shas []string
	for _, ent := range ents {
		if !ent.IsDir() {
			continue
		}
		bh := filepath.Join(dir, ent.Name(), baseHashesName)
		if _, err := os.Stat(bh); err == nil {
			shas = append(shas, ent.Name())
		}
	}
	return shas, nil
}

// resolve picks the best snapshot to consume for a target revision:
//   - same fingerprint (caller already computed it),
//   - base SHA is an ancestor of the target,
//   - nearest such ancestor.
//
// Returns (nil, nil) when no compatible snapshot exists — the caller falls
// back to a cold run. This is the fail-safe from RFC §5.2.
func resolve(store, fingerprint, targetSHA string, git gitClient) (*entry, error) {
	shas, err := listBaseSHAs(store, fingerprint)
	if err != nil {
		return nil, err
	}
	var candidates []candidate
	for _, sha := range shas {
		ok, err := git.isAncestor(sha, targetSHA)
		if err != nil {
			return nil, err
		}
		if !ok {
			continue
		}
		dist, err := git.distance(sha, targetSHA)
		if err != nil {
			return nil, err
		}
		candidates = append(candidates, candidate{BaseSHA: sha, Distance: dist})
	}
	best := pickNearestAncestor(candidates)
	if best == nil {
		return nil, nil
	}
	return &entry{Dir: entryDir(store, fingerprint, best.BaseSHA), BaseSHA: best.BaseSHA}, nil
}

func mustAbs(p string) (string, error) {
	abs, err := filepath.Abs(p)
	if err != nil {
		return "", fmt.Errorf("resolving path %q: %w", p, err)
	}
	return abs, nil
}
