package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"testing"
)

func captureStdout(t *testing.T, fn func()) string {
	t.Helper()
	old := os.Stdout
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatalf("pipe: %v", err)
	}
	os.Stdout = w
	t.Cleanup(func() { os.Stdout = old })

	done := make(chan []byte)
	go func() {
		var buf bytes.Buffer
		_, _ = io.Copy(&buf, r)
		done <- buf.Bytes()
	}()

	fn()
	_ = w.Close()
	return string(<-done)
}

func captureStderr(t *testing.T, fn func()) string {
	t.Helper()
	old := os.Stderr
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatalf("pipe: %v", err)
	}
	os.Stderr = w
	t.Cleanup(func() { os.Stderr = old })

	done := make(chan []byte)
	go func() {
		var buf bytes.Buffer
		_, _ = io.Copy(&buf, r)
		done <- buf.Bytes()
	}()

	fn()
	_ = w.Close()
	return string(<-done)
}

func TestOutputJSONIndentsAndAppendsNewline(t *testing.T) {
	out := captureStdout(t, func() {
		_ = outputJSON(map[string]string{"key": "value"})
	})
	if out != "{\n  \"key\": \"value\"\n}\n" {
		t.Errorf("unexpected output: %q", out)
	}
}

func TestOutputJSONEmptySliceNotNull(t *testing.T) {
	out := captureStdout(t, func() {
		// Pre-allocated empty slice should serialize as [] not null.
		// Documented in the shed reference; tests pin the behavior so
		// callers building list responses don't accidentally regress.
		_ = outputJSON(make([]string, 0))
	})
	if out != "[]\n" {
		t.Errorf("expected []\\n, got %q", out)
	}
}

func TestOutputJSONNilSliceIsNull(t *testing.T) {
	out := captureStdout(t, func() {
		var s []string
		_ = outputJSON(s)
	})
	if out != "null\n" {
		t.Errorf("expected null\\n, got %q", out)
	}
}

func TestActionResultOmitsEmptyFields(t *testing.T) {
	out := captureStdout(t, func() {
		_ = outputJSON(ActionResult{Status: "ok", Action: "removed", Name: "tab1"})
	})
	var parsed map[string]any
	if err := json.Unmarshal([]byte(out), &parsed); err != nil {
		t.Fatalf("invalid JSON: %v", err)
	}
	if _, ok := parsed["details"]; ok {
		t.Error("expected details to be omitted when empty")
	}
	if parsed["status"] != "ok" || parsed["action"] != "removed" || parsed["name"] != "tab1" {
		t.Errorf("unexpected payload: %+v", parsed)
	}
}

func TestActionResultIncludesDetailsWhenPresent(t *testing.T) {
	out := captureStdout(t, func() {
		_ = outputJSON(ActionResult{
			Status: "ok", Action: "set-state", Name: "5",
			Details: map[string]string{"state": "running"},
		})
	})
	var parsed map[string]any
	if err := json.Unmarshal([]byte(out), &parsed); err != nil {
		t.Fatalf("invalid JSON: %v", err)
	}
	d, ok := parsed["details"].(map[string]any)
	if !ok {
		t.Fatalf("expected details map, got %T", parsed["details"])
	}
	if d["state"] != "running" {
		t.Errorf("expected state=running, got %v", d["state"])
	}
}

func TestOutputErrorWritesJSONToStderr(t *testing.T) {
	out := captureStderr(t, func() {
		_ = outputError(errors.New("something went wrong"))
	})
	var parsed struct {
		Error string `json:"error"`
	}
	if err := json.Unmarshal([]byte(out), &parsed); err != nil {
		t.Fatalf("invalid JSON: %v\noutput: %s", err, out)
	}
	if parsed.Error != "something went wrong" {
		t.Errorf("expected error message, got %q", parsed.Error)
	}
}

func TestPrintSuccessNoOpInJSONMode(t *testing.T) {
	prev := clientCtx.JSON
	clientCtx.JSON = true
	t.Cleanup(func() { clientCtx.JSON = prev })

	out := captureStdout(t, func() {
		printSuccess("nothing %s", "here")
	})
	if out != "" {
		t.Errorf("expected no output in JSON mode, got %q", out)
	}
}

func TestPrintSuccessRendersInHumanMode(t *testing.T) {
	prev := clientCtx.JSON
	clientCtx.JSON = false
	t.Cleanup(func() { clientCtx.JSON = prev })

	out := captureStdout(t, func() {
		printSuccess("done %d", 7)
	})
	want := fmt.Sprintf("✓ done %d\n", 7)
	if out != want {
		t.Errorf("got %q, want %q", out, want)
	}
}
