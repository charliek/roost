package config

import (
	"os"
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
		// Don't hard-code the default ~/.config layout: a developer with
		// XDG_CONFIG_HOME set would otherwise spuriously fail this test.
		// TestResolveConfigDirRespectsXDG covers the env-var path explicitly.
		if filepath.Base(p.ConfigDir) != AppName {
			t.Errorf("Mac ConfigDir basename: got %q, want %q", filepath.Base(p.ConfigDir), AppName)
		}
		if !strings.Contains(p.DataDir, "Library/Application Support/Roost") {
			t.Errorf("Mac DataDir should still be Application Support, got %q", p.DataDir)
		}
		if !strings.Contains(p.RuntimeDir, "Library/Application Support/Roost") {
			t.Errorf("Mac RuntimeDir should still be Application Support, got %q", p.RuntimeDir)
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
	if filepath.Base(p.ConfigFile()) != "config.conf" {
		t.Errorf("ConfigFile basename: got %q, want config.conf", filepath.Base(p.ConfigFile()))
	}
}

func TestResolveConfigDirRespectsXDG(t *testing.T) {
	t.Setenv("XDG_CONFIG_HOME", "/tmp/x")
	p, err := Resolve()
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	want := filepath.Join("/tmp/x", AppName)
	if p.ConfigDir != want {
		t.Errorf("XDG_CONFIG_HOME=/tmp/x: ConfigDir got %q, want %q", p.ConfigDir, want)
	}
	if p.ConfigFile() != filepath.Join(want, "config.conf") {
		t.Errorf("ConfigFile under XDG: got %q", p.ConfigFile())
	}
}

func TestResolveConfigDirDefaultsToHomeDotConfig(t *testing.T) {
	t.Setenv("XDG_CONFIG_HOME", "")
	p, err := Resolve()
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	home, err := os.UserHomeDir()
	if err != nil {
		t.Fatalf("UserHomeDir: %v", err)
	}
	want := filepath.Join(home, ".config", AppName)
	if p.ConfigDir != want {
		t.Errorf("default ConfigDir: got %q, want %q", p.ConfigDir, want)
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
