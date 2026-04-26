package main

import (
	"context"
	"log/slog"
	"os"
	"strings"
)

// installLogFilter routes GLib warnings through a slog handler that
// drops the GTK theme-parser noise from the system Adwaita stylesheet.
//
// gotk4 installs g_log_set_writer_func at init time, forwarding GLib
// records to slog.Default(). We swap the default with a wrapper that
// suppresses one well-known noise source. Everything else passes
// through unchanged.
func installLogFilter() {
	base := slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})
	slog.SetDefault(slog.New(filterHandler{inner: base}))
}

type filterHandler struct {
	inner slog.Handler
}

func (h filterHandler) Enabled(ctx context.Context, lvl slog.Level) bool {
	return h.inner.Enabled(ctx, lvl)
}

func (h filterHandler) Handle(ctx context.Context, r slog.Record) error {
	if r.Level == slog.LevelWarn && strings.HasPrefix(r.Message, "Theme parser") {
		var domain string
		r.Attrs(func(a slog.Attr) bool {
			if a.Key == "glib_domain" {
				domain = a.Value.String()
				return false
			}
			return true
		})
		if domain == "Gtk" {
			return nil
		}
	}
	return h.inner.Handle(ctx, r)
}

func (h filterHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	return filterHandler{inner: h.inner.WithAttrs(attrs)}
}

func (h filterHandler) WithGroup(name string) slog.Handler {
	return filterHandler{inner: h.inner.WithGroup(name)}
}
