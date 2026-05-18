package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"time"

	"github.com/charliek/roost/internal/ipc"
)

// IPCClient is a thin wrapper around internal/ipc.Dial that applies
// the global --socket and --timeout flags and returns hint-bearing
// errors when the GUI is unreachable.
type IPCClient struct {
	SocketPath string
	Timeout    time.Duration
}

// newClient builds an IPCClient from clientCtx (populated by
// PersistentPreRunE). Subcommands call this once at the top of RunE.
func newClient() *IPCClient {
	return &IPCClient{
		SocketPath: clientCtx.SocketPath,
		Timeout:    clientCtx.Timeout,
	}
}

// call sends a single JSON-RPC request and returns the raw response.
// On connect failure it wraps the error with the "is the Roost GUI
// running?" hint so users get an actionable message.
func (c *IPCClient) call(method string, params any) (ipc.Response, error) {
	body, err := json.Marshal(params)
	if err != nil {
		return ipc.Response{}, fmt.Errorf("marshal %s: %w", method, err)
	}
	timeout := c.Timeout
	if timeout <= 0 {
		timeout = 3 * time.Second
	}
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	resp, err := ipc.Dial(ctx, c.SocketPath, ipc.Request{
		ID:     "1",
		Method: method,
		Params: body,
	})
	if err != nil {
		// ECONNREFUSED / ENOENT mean the GUI isn't running. Wrap with
		// a hint; other errors (timeouts, bad path) pass through.
		if isConnectErr(err) {
			return resp, fmt.Errorf("connect %s: %w (is the Roost GUI running?)", c.SocketPath, err)
		}
		return resp, fmt.Errorf("connect %s: %w", c.SocketPath, err)
	}
	if !resp.OK {
		if resp.Error != nil {
			return resp, fmt.Errorf("%s: %s: %s", method, resp.Error.Code, resp.Error.Message)
		}
		return resp, fmt.Errorf("%s: request failed", method)
	}
	return resp, nil
}

// callTyped sends a request and decodes the typed result. Pass &out as
// a pointer to the typed payload (e.g., *ipc.TabListResult).
func (c *IPCClient) callTyped(method string, params any, out any) error {
	resp, err := c.call(method, params)
	if err != nil {
		return err
	}
	if resp.Result == nil {
		return nil
	}
	// resp.Result is a map[string]any from the JSON roundtrip; round-
	// trip through JSON to land it in the typed struct. Cheap and
	// avoids a custom decoder.
	raw, err := json.Marshal(resp.Result)
	if err != nil {
		return fmt.Errorf("marshal %s result: %w", method, err)
	}
	if err := json.Unmarshal(raw, out); err != nil {
		return fmt.Errorf("unmarshal %s result: %w", method, err)
	}
	return nil
}

// Notify sends a desktop notification through the GUI.
func (c *IPCClient) Notify(tabID int64, title, body string) error {
	_, err := c.call(ipc.MethodNotificationCreate, ipc.NotifyParams{
		TabID: tabID, Title: title, Body: body,
	})
	return err
}

// Identify returns the Identity payload describing the running GUI.
func (c *IPCClient) Identify() (ipc.Identity, error) {
	var out ipc.Identity
	err := c.callTyped(ipc.MethodSystemIdentify, struct{}{}, &out)
	return out, err
}

// TabList returns the project-grouped tab tree.
func (c *IPCClient) TabList() (ipc.TabListResult, error) {
	var out ipc.TabListResult
	err := c.callTyped(ipc.MethodTabList, struct{}{}, &out)
	return out, err
}

// TabFocus switches the GUI to the named tab.
func (c *IPCClient) TabFocus(tabID int64) (ipc.TabFocusResult, error) {
	var out ipc.TabFocusResult
	err := c.callTyped(ipc.MethodTabFocus, ipc.TabFocusParams{TabID: tabID}, &out)
	return out, err
}

// TabSetTitle renames a tab.
func (c *IPCClient) TabSetTitle(tabID int64, title string) error {
	_, err := c.call(ipc.MethodTabSetTitle, ipc.SetTitleParams{TabID: tabID, Title: title})
	return err
}

// TabSetState writes a sticky agent state on a tab.
func (c *IPCClient) TabSetState(tabID int64, state string) error {
	_, err := c.call(ipc.MethodTabSetState, ipc.SetStateParams{TabID: tabID, State: state})
	return err
}

// isConnectErr returns true for the "no listener" failure modes that
// indicate the GUI isn't running. Not exhaustive — false negatives
// just mean the user gets a less-friendly error.
func isConnectErr(err error) bool {
	if err == nil {
		return false
	}
	var opErr *net.OpError
	if errors.As(err, &opErr) {
		return true
	}
	if errors.Is(err, os.ErrNotExist) {
		return true
	}
	return false
}
