// Package ipc is Roost's local IPC layer. The GUI app runs a Unix-socket
// server that the companion roost-cli binary (or any other client)
// talks to via newline-delimited JSON-RPC. Modeled on cmux v2.
package ipc

import "encoding/json"

// Request is one JSON-RPC call from a client.
type Request struct {
	ID     string          `json:"id"`
	Method string          `json:"method"`
	Params json.RawMessage `json:"params,omitempty"`
}

// Response is the server's reply for a single request id.
type Response struct {
	ID     string      `json:"id"`
	OK     bool        `json:"ok"`
	Result interface{} `json:"result,omitempty"`
	Error  *RPCError   `json:"error,omitempty"`
}

// RPCError is the standard error envelope for failed requests.
type RPCError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// Method names. Add new ones here to keep the surface in one place.
const (
	MethodNotificationCreate   = "notification.create"
	MethodTabSetTitle          = "tab.set_title"
	MethodTabFocus             = "tab.focus"
	MethodTabList              = "tab.list"
	MethodTabSetState          = "tab.set_state"
	MethodTabClearNotification = "tab.clear_notification"
	MethodSystemIdentify       = "system.identify"
	MethodSystemSetHookActive  = "system.set_hook_active"
)

// NotifyParams is the body of a notification.create call. TabID is
// optional; the CLI fills it from $ROOST_TAB_ID when omitted.
type NotifyParams struct {
	TabID int64  `json:"tab_id,omitempty"`
	Title string `json:"title"`
	Body  string `json:"body,omitempty"`
}

// SetTitleParams is the body of a tab.set_title call.
type SetTitleParams struct {
	TabID int64  `json:"tab_id,omitempty"`
	Title string `json:"title"`
}

// Identity is the result of a system.identify call.
type Identity struct {
	SocketPath      string `json:"socket"`
	PID             int    `json:"pid"`
	ActiveProjectID int64  `json:"active_project_id"`
	ActiveTabID     int64  `json:"active_tab_id"`
}

// TabFocusParams is the body of a tab.focus call. Switches the active
// project (if needed), selects the named tab, raises the window, and
// grabs DA focus.
type TabFocusParams struct {
	TabID int64 `json:"tab_id"`
}

// TabFocusResult returns the previously focused (project, tab) so a
// caller can implement "focus back" without holding state.
type TabFocusResult struct {
	PreviousProjectID int64 `json:"previous_project_id"`
	PreviousTabID     int64 `json:"previous_tab_id"`
}

// SetStateParams sets the sticky per-tab agent state. State is one of
// "none", "running", "needs_input", "idle"; anything else returns
// bad_request.
type SetStateParams struct {
	TabID int64  `json:"tab_id"`
	State string `json:"state"`
}

// ClearNotificationParams clears the per-tab "has pending
// notification" flag. Used by the prompt-submit hook so a fresh
// prompt clears any stale awaiting-input badge.
type ClearNotificationParams struct {
	TabID int64 `json:"tab_id"`
}

// SetHookActiveParams toggles the per-tab hook-session-active flag.
// While true, raw OSC 9/777 from inside the tab is suppressed —
// hook-driven agents own the notification surface.
type SetHookActiveParams struct {
	TabID  int64 `json:"tab_id"`
	Active bool  `json:"active"`
}

// TabListResult is the result of a tab.list call. Tabs are grouped
// by project in display order; tabs within a project are in visual
// order.
type TabListResult struct {
	Projects []TabListProject `json:"projects"`
}

// TabListProject is one project's snapshot inside TabListResult.
type TabListProject struct {
	ID   int64        `json:"id"`
	Name string       `json:"name"`
	Tabs []TabListTab `json:"tabs"`
}

// TabListTab is one tab's snapshot inside TabListProject.
type TabListTab struct {
	ID              int64  `json:"id"`
	Title           string `json:"title"`
	AgentState      string `json:"agent_state"`
	HasNotification bool   `json:"has_notification"`
	IsActive        bool   `json:"is_active"`
}

// Error codes — kept short and stable since clients match on them.
const (
	CodeBadRequest = "bad_request"
	CodeNotFound   = "not_found"
	CodeInternal   = "internal"
)

// NewError is a small ergonomic helper for handlers.
func NewError(code, msg string) *RPCError { return &RPCError{Code: code, Message: msg} }
