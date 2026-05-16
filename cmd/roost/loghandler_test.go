package main

import (
	"bytes"
	"context"
	"log/slog"
	"strings"
	"testing"
	"time"
)

func TestFilterHandler(t *testing.T) {
	cases := []struct {
		name string
		// rec describes the log record the filter sees.
		level slog.Level
		msg   string
		attrs []slog.Attr
		// drop is true when we expect the record to be filtered out.
		drop bool
	}{
		{
			name:  "theme parser gtk dropped",
			level: slog.LevelWarn,
			msg:   "Theme parser warning: foo",
			attrs: []slog.Attr{slog.String("glib_domain", "Gtk")},
			drop:  true,
		},
		{
			name:  "theme parser other domain passes",
			level: slog.LevelWarn,
			msg:   "Theme parser warning: foo",
			attrs: []slog.Attr{slog.String("glib_domain", "GLib")},
		},
		{
			name:  "schema source lookup error dropped",
			level: slog.LevelError,
			msg:   "g_settings_schema_source_lookup: assertion 'source != NULL' failed",
			attrs: []slog.Attr{slog.String("glib_domain", "GLib-GIO")},
			drop:  true,
		},
		{
			name:  "schema source lookup warn dropped",
			level: slog.LevelWarn,
			msg:   "g_settings_schema_source_lookup: something",
			drop:  true,
		},
		{
			name:  "schema source info passes",
			level: slog.LevelInfo,
			msg:   "g_settings_schema_source_lookup: info-level shouldn't be filtered",
		},
		{
			name:  "unrelated info passes",
			level: slog.LevelInfo,
			msg:   "ipc listening",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			var buf bytes.Buffer
			base := slog.NewTextHandler(&buf, &slog.HandlerOptions{Level: slog.LevelDebug})
			h := filterHandler{inner: base}

			r := slog.NewRecord(time.Time{}, tc.level, tc.msg, 0)
			r.AddAttrs(tc.attrs...)
			if err := h.Handle(context.Background(), r); err != nil {
				t.Fatalf("Handle returned err: %v", err)
			}

			got := buf.String()
			if tc.drop && got != "" {
				t.Errorf("expected record to be dropped; got output %q", got)
			}
			if !tc.drop && !strings.Contains(got, tc.msg) {
				t.Errorf("expected record to pass through; got output %q", got)
			}
		})
	}
}

