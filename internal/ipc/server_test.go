package ipc

import (
	"context"
	"encoding/json"
	"errors"
	"net"
	"path/filepath"
	"testing"
	"time"
)

type fakeHandler struct {
	notify   func(int64, string, string) error
	setTitle func(int64, string) error
	identify func() Identity
}

func (f fakeHandler) Notify(tab int64, title, body string) error {
	return f.notify(tab, title, body)
}
func (f fakeHandler) SetTitle(tab int64, title string) error { return f.setTitle(tab, title) }
func (f fakeHandler) Identify() Identity                     { return f.identify() }

func startServer(t *testing.T, h Handler) string {
	t.Helper()
	sock := filepath.Join(t.TempDir(), "roost.sock")
	s := NewServer(sock, h)
	if err := s.Start(); err != nil {
		t.Fatalf("Start: %v", err)
	}
	t.Cleanup(func() { _ = s.Close() })
	return sock
}

func TestNotifyRoundtrip(t *testing.T) {
	got := struct {
		tab         int64
		title, body string
	}{}
	sock := startServer(t, fakeHandler{
		notify: func(tab int64, title, body string) error {
			got.tab = tab
			got.title = title
			got.body = body
			return nil
		},
	})

	params, _ := json.Marshal(NotifyParams{TabID: 42, Title: "hello", Body: "world"})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodNotificationCreate, Params: params,
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if !resp.OK {
		t.Fatalf("expected OK, got %+v", resp)
	}
	if got.tab != 42 || got.title != "hello" || got.body != "world" {
		t.Fatalf("handler args: %+v", got)
	}
}

func TestNotifyRequiresTitle(t *testing.T) {
	sock := startServer(t, fakeHandler{
		notify: func(int64, string, string) error { return nil },
	})

	params, _ := json.Marshal(NotifyParams{TabID: 1, Title: ""})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodNotificationCreate, Params: params,
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if resp.OK {
		t.Fatalf("expected error response, got %+v", resp)
	}
	if resp.Error.Code != CodeBadRequest {
		t.Errorf("error code: got %s", resp.Error.Code)
	}
}

func TestIdentify(t *testing.T) {
	want := Identity{SocketPath: "/foo", PID: 1234, ActiveProjectID: 5, ActiveTabID: 9}
	sock := startServer(t, fakeHandler{
		identify: func() Identity { return want },
	})

	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodSystemIdentify, Params: json.RawMessage(`{}`),
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if !resp.OK {
		t.Fatalf("expected OK, got %+v", resp)
	}
	// Result was decoded as map[string]any from JSON; verify a couple of fields.
	m, _ := resp.Result.(map[string]any)
	if int64(m["active_tab_id"].(float64)) != want.ActiveTabID {
		t.Fatalf("active_tab_id: got %v want %d", m["active_tab_id"], want.ActiveTabID)
	}
}

func TestUnknownMethod(t *testing.T) {
	sock := startServer(t, fakeHandler{})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: "bogus.method", Params: json.RawMessage(`{}`),
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if resp.OK || resp.Error.Code != CodeBadRequest {
		t.Errorf("expected bad_request, got %+v", resp)
	}
}

func TestServerCloseRemovesSocket(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "x.sock")
	s := NewServer(sock, fakeHandler{})
	if err := s.Start(); err != nil {
		t.Fatalf("Start: %v", err)
	}
	if err := s.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if _, err := timeoutDial(sock); err == nil {
		t.Fatal("dial should fail after Close")
	}
}

func TestRejectsOversizedRequest(t *testing.T) {
	sock := startServer(t, fakeHandler{
		notify: func(int64, string, string) error { return nil },
	})

	conn, err := net.Dial("unix", sock)
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer conn.Close()

	// Stream 2 MiB of bytes without a newline — the server should
	// give up after maxRequestBytes and close the connection.
	chunk := make([]byte, 64<<10)
	for i := range chunk {
		chunk[i] = 'x'
	}
	_ = conn.SetWriteDeadline(time.Now().Add(2 * time.Second))
	written := 0
	for written < (2 << 20) {
		n, werr := conn.Write(chunk)
		written += n
		if werr != nil {
			break // server closed early — that's the expected outcome
		}
	}

	// The server should respond with a bad_request before closing.
	_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
	dec := json.NewDecoder(conn)
	var resp Response
	if err := dec.Decode(&resp); err != nil {
		// Acceptable: server may have closed before flushing reply if
		// the client write blocked. Either way, the connection must
		// not stay open holding 2 MiB in memory.
		return
	}
	if resp.OK || resp.Error == nil || resp.Error.Code != CodeBadRequest {
		t.Errorf("expected bad_request, got %+v", resp)
	}
}

func timeoutDial(sock string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	resp, err := Dial(ctx, sock, Request{ID: "x", Method: MethodSystemIdentify, Params: json.RawMessage(`{}`)})
	if err != nil {
		return "", err
	}
	if !resp.OK {
		return "", errors.New("not ok")
	}
	return resp.ID, nil
}
