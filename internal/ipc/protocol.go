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
	MethodNotificationCreate = "notification.create"
	MethodTabSetTitle        = "tab.set_title"
	MethodSystemIdentify     = "system.identify"
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

// Error codes — kept short and stable since clients match on them.
const (
	CodeBadRequest = "bad_request"
	CodeNotFound   = "not_found"
	CodeInternal   = "internal"
)

// NewError is a small ergonomic helper for handlers.
func NewError(code, msg string) *RPCError { return &RPCError{Code: code, Message: msg} }
