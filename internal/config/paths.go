// Package config resolves filesystem paths for Roost: where the SQLite
// database lives, where the config file lives, where the runtime socket
// lives. Cross-platform: XDG-style on Linux, Application Support on Mac.
package config

import (
	"errors"
	"os"
	"path/filepath"
	"runtime"
)

// AppName is the directory/file basename. Lower-case for Linux/XDG; Mac
// uses Title-case Application Support but we mirror the lower-case dir
// name so the binary is unambiguous in `~/.config/roost/`.
const AppName = "roost"

// Paths bundles the standard Roost filesystem locations. Resolved once at
// startup; callers should not derive their own.
type Paths struct {
	// ConfigDir holds the user-editable config file (config.toml). Created
	// on first launch.
	ConfigDir string

	// DataDir holds persistent state (the SQLite DB, scrollback caches if
	// any). On Mac this is the same as ConfigDir; on Linux it follows
	// $XDG_DATA_HOME so backups can target it independently of config.
	DataDir string

	// RuntimeDir holds the Unix socket for the companion CLI. On Mac it's
	// inside Application Support; on Linux it follows $XDG_RUNTIME_DIR
	// when set (a tmpfs that's auto-cleaned on logout).
	RuntimeDir string
}

// DBPath is where the SQLite database lives.
func (p Paths) DBPath() string { return filepath.Join(p.DataDir, "roost.db") }

// ConfigFile is where the user-editable TOML config lives.
func (p Paths) ConfigFile() string { return filepath.Join(p.ConfigDir, "config.toml") }

// SocketPath is where the companion CLI's Unix socket lives.
func (p Paths) SocketPath() string { return filepath.Join(p.RuntimeDir, "roost.sock") }

// Resolve returns the standard Paths for the current platform. Does not
// create the directories — call EnsureDirs to do that.
func Resolve() (Paths, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return Paths{}, err
	}

	switch runtime.GOOS {
	case "darwin":
		base := filepath.Join(home, "Library", "Application Support", "Roost")
		return Paths{
			ConfigDir:  base,
			DataDir:    base,
			RuntimeDir: base,
		}, nil

	case "linux":
		configDir := envOr("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
		dataDir := envOr("XDG_DATA_HOME", filepath.Join(home, ".local", "share"))
		runtimeDir := os.Getenv("XDG_RUNTIME_DIR")
		if runtimeDir == "" {
			// XDG_RUNTIME_DIR unset (e.g. ssh session): fall back to data dir.
			runtimeDir = filepath.Join(dataDir, AppName)
		} else {
			runtimeDir = filepath.Join(runtimeDir, AppName)
		}
		return Paths{
			ConfigDir:  filepath.Join(configDir, AppName),
			DataDir:    filepath.Join(dataDir, AppName),
			RuntimeDir: runtimeDir,
		}, nil

	default:
		return Paths{}, errors.New("config: unsupported platform " + runtime.GOOS)
	}
}

// EnsureDirs creates the config, data, and runtime directories with 0700
// permissions if they don't already exist.
func (p Paths) EnsureDirs() error {
	for _, d := range []string{p.ConfigDir, p.DataDir, p.RuntimeDir} {
		if err := os.MkdirAll(d, 0o700); err != nil {
			return err
		}
	}
	return nil
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}
