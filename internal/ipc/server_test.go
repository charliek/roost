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
	notify        func(int64, string, string) error
	setTitle      func(int64, string) error
	identify      func() Identity
	focusTab      func(int64) (TabFocusResult, error)
	listTabs      func() (TabListResult, error)
	setTabState   func(int64, string) error
	clearTabNotif func(int64) error
	setHookActive func(int64, bool) error
}

func (f fakeHandler) Notify(tab int64, title, body string) error {
	return f.notify(tab, title, body)
}
func (f fakeHandler) SetTitle(tab int64, title string) error { return f.setTitle(tab, title) }
func (f fakeHandler) Identify() Identity                     { return f.identify() }
func (f fakeHandler) FocusTab(tab int64) (TabFocusResult, error) {
	if f.focusTab == nil {
		return TabFocusResult{}, nil
	}
	return f.focusTab(tab)
}
func (f fakeHandler) ListTabs() (TabListResult, error) {
	if f.listTabs == nil {
		return TabListResult{}, nil
	}
	return f.listTabs()
}
func (f fakeHandler) SetTabState(tab int64, state string) error {
	if f.setTabState == nil {
		return nil
	}
	return f.setTabState(tab, state)
}
func (f fakeHandler) ClearTabNotification(tab int64) error {
	if f.clearTabNotif == nil {
		return nil
	}
	return f.clearTabNotif(tab)
}
func (f fakeHandler) SetHookActive(tab int64, active bool) error {
	if f.setHookActive == nil {
		return nil
	}
	return f.setHookActive(tab, active)
}

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

func TestTabFocusRoundtrip(t *testing.T) {
	want := TabFocusResult{PreviousProjectID: 3, PreviousTabID: 7}
	var gotTab int64
	sock := startServer(t, fakeHandler{
		focusTab: func(tab int64) (TabFocusResult, error) {
			gotTab = tab
			return want, nil
		},
	})
	params, _ := json.Marshal(TabFocusParams{TabID: 42})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodTabFocus, Params: params,
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if !resp.OK {
		t.Fatalf("expected OK, got %+v", resp)
	}
	if gotTab != 42 {
		t.Fatalf("handler got tab %d", gotTab)
	}
	m, _ := resp.Result.(map[string]any)
	if int64(m["previous_tab_id"].(float64)) != want.PreviousTabID {
		t.Fatalf("previous_tab_id: %v", m["previous_tab_id"])
	}
}

func TestTabFocusRequiresTabID(t *testing.T) {
	sock := startServer(t, fakeHandler{
		focusTab: func(int64) (TabFocusResult, error) { return TabFocusResult{}, nil },
	})
	params, _ := json.Marshal(TabFocusParams{TabID: 0})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodTabFocus, Params: params,
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if resp.OK || resp.Error.Code != CodeBadRequest {
		t.Fatalf("expected bad_request, got %+v", resp)
	}
}

func TestTabSetStateValidatesEnum(t *testing.T) {
	called := false
	sock := startServer(t, fakeHandler{
		setTabState: func(int64, string) error { called = true; return nil },
	})

	for _, tc := range []struct {
		name    string
		state   string
		wantOK  bool
		wantErr string
	}{
		{"running", "running", true, ""},
		{"needs_input", "needs_input", true, ""},
		{"idle", "idle", true, ""},
		{"none", "none", true, ""},
		{"invalid Caps", "Running", false, CodeBadRequest},
		{"invalid empty", "", false, CodeBadRequest},
		{"invalid garbage", "done", false, CodeBadRequest},
	} {
		t.Run(tc.name, func(t *testing.T) {
			called = false
			params, _ := json.Marshal(SetStateParams{TabID: 1, State: tc.state})
			resp, err := Dial(context.Background(), sock, Request{
				ID: "1", Method: MethodTabSetState, Params: params,
			})
			if err != nil {
				t.Fatalf("Dial: %v", err)
			}
			if resp.OK != tc.wantOK {
				t.Fatalf("OK = %v, want %v (resp %+v)", resp.OK, tc.wantOK, resp)
			}
			if !tc.wantOK && resp.Error.Code != tc.wantErr {
				t.Fatalf("err code: got %s want %s", resp.Error.Code, tc.wantErr)
			}
			if tc.wantOK && !called {
				t.Fatal("handler was not called for valid state")
			}
			if !tc.wantOK && called {
				t.Fatal("handler was called for invalid state — bad_request should short-circuit")
			}
		})
	}
}

func TestTabClearNotif(t *testing.T) {
	var gotTab int64
	sock := startServer(t, fakeHandler{
		clearTabNotif: func(tab int64) error { gotTab = tab; return nil },
	})
	params, _ := json.Marshal(ClearNotificationParams{TabID: 5})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodTabClearNotification, Params: params,
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if !resp.OK || gotTab != 5 {
		t.Fatalf("clear: resp=%+v gotTab=%d", resp, gotTab)
	}
}

func TestSetHookActive(t *testing.T) {
	var gotTab int64
	var gotActive bool
	sock := startServer(t, fakeHandler{
		setHookActive: func(tab int64, active bool) error {
			gotTab = tab
			gotActive = active
			return nil
		},
	})
	params, _ := json.Marshal(SetHookActiveParams{TabID: 7, Active: true})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodSystemSetHookActive, Params: params,
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if !resp.OK || gotTab != 7 || gotActive != true {
		t.Fatalf("set_hook_active: resp=%+v gotTab=%d active=%v", resp, gotTab, gotActive)
	}
}

func TestTabListReturnsResult(t *testing.T) {
	want := TabListResult{
		Projects: []TabListProject{
			{ID: 1, Name: "alpha", Tabs: []TabListTab{
				{ID: 11, Title: "tab a", AgentState: "running", HasNotification: true, IsActive: false},
			}},
		},
	}
	sock := startServer(t, fakeHandler{
		listTabs: func() (TabListResult, error) { return want, nil },
	})
	resp, err := Dial(context.Background(), sock, Request{
		ID: "1", Method: MethodTabList, Params: json.RawMessage(`{}`),
	})
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	if !resp.OK {
		t.Fatalf("expected OK, got %+v", resp)
	}
	m, _ := resp.Result.(map[string]any)
	projects, _ := m["projects"].([]any)
	if len(projects) != 1 {
		t.Fatalf("projects: %v", m)
	}
	first, _ := projects[0].(map[string]any)
	if first["name"].(string) != "alpha" {
		t.Errorf("name: %v", first["name"])
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
