package config

import (
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestResolve(t *testing.T) {
	p, err := Resolve()
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}

	if p.ConfigDir == "" || p.DataDir == "" || p.RuntimeDir == "" {
		t.Fatalf("expected all dirs populated, got %+v", p)
	}

	switch runtime.GOOS {
	case "darwin":
		if !strings.Contains(p.ConfigDir, "Library/Application Support/Roost") {
			t.Errorf("Mac ConfigDir should contain 'Library/Application Support/Roost', got %q", p.ConfigDir)
		}
		if p.ConfigDir != p.DataDir {
			t.Errorf("Mac ConfigDir and DataDir should be equal, got %q vs %q", p.ConfigDir, p.DataDir)
		}
	case "linux":
		if !strings.Contains(p.ConfigDir, AppName) {
			t.Errorf("Linux ConfigDir should contain %q, got %q", AppName, p.ConfigDir)
		}
	}

	if filepath.Base(p.DBPath()) != "roost.db" {
		t.Errorf("DBPath basename: got %q, want roost.db", filepath.Base(p.DBPath()))
	}
	if filepath.Base(p.SocketPath()) != "roost.sock" {
		t.Errorf("SocketPath basename: got %q, want roost.sock", filepath.Base(p.SocketPath()))
	}
}

func TestEnsureDirs(t *testing.T) {
	tmp := t.TempDir()
	p := Paths{
		ConfigDir:  filepath.Join(tmp, "cfg"),
		DataDir:    filepath.Join(tmp, "data"),
		RuntimeDir: filepath.Join(tmp, "run"),
	}
	if err := p.EnsureDirs(); err != nil {
		t.Fatalf("EnsureDirs: %v", err)
	}
	for _, d := range []string{p.ConfigDir, p.DataDir, p.RuntimeDir} {
		if _, err := filepath.EvalSymlinks(d); err != nil {
			t.Errorf("dir not created: %s: %v", d, err)
		}
	}
}
