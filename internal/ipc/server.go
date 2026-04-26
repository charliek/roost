package ipc

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net"
	"os"
	"sync"
)

// Handler is implemented by the App. Each method maps to one RPC.
// Implementations are called from goroutines and must be safe for
// concurrent use; touching GTK widgets from inside requires marshalling
// via glib.IdleAdd.
type Handler interface {
	Notify(tabID int64, title, body string) error
	SetTitle(tabID int64, title string) error
	Identify() Identity
}

// maxRequestBytes is the per-request size cap. Real requests
// (notification.create, system.identify, tab.set_title) are well under
// 1 KiB; the cap exists to bound memory if a misbehaving client streams
// without ever sending a newline. Mode 0600 keeps the surface
// local-user-only, but we still don't want a stuck client to OOM us.
const maxRequestBytes = 1 << 20 // 1 MiB

// Server listens on a Unix socket and dispatches Requests to a Handler.
type Server struct {
	socketPath string
	handler    Handler

	mu       sync.Mutex
	listener net.Listener
	stopped  bool
}

// NewServer wires a handler to a socket path. Listen+accept start when
// you call Start.
func NewServer(socketPath string, h Handler) *Server {
	return &Server{socketPath: socketPath, handler: h}
}

// Start begins listening. Returns once the listener is up; the accept
// loop runs in its own goroutine. Stop with Close.
//
// If the socket file already exists (stale lock from a prior crash),
// Start removes it and tries again. We assume a single Roost instance.
func (s *Server) Start() error {
	if err := os.RemoveAll(s.socketPath); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	l, err := net.Listen("unix", s.socketPath)
	if err != nil {
		return err
	}
	if err := os.Chmod(s.socketPath, 0o600); err != nil {
		_ = l.Close()
		return err
	}
	s.mu.Lock()
	s.listener = l
	s.mu.Unlock()

	go s.acceptLoop(l)
	return nil
}

// Close shuts down the listener. Pending connections drain naturally.
func (s *Server) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.stopped {
		return nil
	}
	s.stopped = true
	if s.listener != nil {
		_ = s.listener.Close()
	}
	_ = os.Remove(s.socketPath)
	return nil
}

func (s *Server) acceptLoop(l net.Listener) {
	for {
		conn, err := l.Accept()
		if err != nil {
			s.mu.Lock()
			stopped := s.stopped
			s.mu.Unlock()
			if stopped {
				return
			}
			slog.Warn("ipc accept", "err", err)
			return
		}
		go s.serve(conn)
	}
}

func (s *Server) serve(conn net.Conn) {
	defer conn.Close()
	r := bufio.NewReaderSize(conn, 64<<10)
	enc := json.NewEncoder(conn)
	for {
		line, err := readBoundedLine(r, maxRequestBytes)
		if err != nil {
			if errors.Is(err, errRequestTooLarge) {
				_ = enc.Encode(Response{OK: false, Error: NewError(CodeBadRequest, "request exceeds max size")})
				return
			}
			if err != io.EOF {
				slog.Debug("ipc read", "err", err)
			}
			return
		}
		var req Request
		if err := json.Unmarshal(line, &req); err != nil {
			_ = enc.Encode(Response{ID: req.ID, OK: false, Error: NewError(CodeBadRequest, "malformed request")})
			continue
		}
		resp := s.dispatch(req)
		if err := enc.Encode(resp); err != nil {
			return
		}
	}
}

// errRequestTooLarge signals a connection that streamed past the size
// cap without sending a newline. We respond with bad_request and close.
var errRequestTooLarge = errors.New("ipc: request exceeds max size")

// readBoundedLine reads a single newline-terminated request, refusing
// to grow past max bytes. Implemented in terms of bufio.Reader.ReadSlice
// to avoid allocating per-byte; ReadSlice returns ErrBufferFull when the
// underlying buffer fills, at which point we accumulate up to max and
// then bail.
func readBoundedLine(r *bufio.Reader, max int) ([]byte, error) {
	var buf []byte
	for {
		chunk, err := r.ReadSlice('\n')
		if len(buf)+len(chunk) > max {
			return nil, errRequestTooLarge
		}
		buf = append(buf, chunk...)
		if err == nil {
			return buf, nil
		}
		if errors.Is(err, bufio.ErrBufferFull) {
			continue
		}
		return buf, err
	}
}

func (s *Server) dispatch(req Request) Response {
	switch req.Method {
	case MethodNotificationCreate:
		var p NotifyParams
		if err := json.Unmarshal(req.Params, &p); err != nil {
			return Response{ID: req.ID, OK: false, Error: NewError(CodeBadRequest, err.Error())}
		}
		if p.Title == "" {
			return Response{ID: req.ID, OK: false, Error: NewError(CodeBadRequest, "title required")}
		}
		if err := s.handler.Notify(p.TabID, p.Title, p.Body); err != nil {
			return Response{ID: req.ID, OK: false, Error: NewError(CodeInternal, err.Error())}
		}
		return Response{ID: req.ID, OK: true, Result: map[string]any{"delivered": true}}

	case MethodTabSetTitle:
		var p SetTitleParams
		if err := json.Unmarshal(req.Params, &p); err != nil {
			return Response{ID: req.ID, OK: false, Error: NewError(CodeBadRequest, err.Error())}
		}
		if err := s.handler.SetTitle(p.TabID, p.Title); err != nil {
			return Response{ID: req.ID, OK: false, Error: NewError(CodeInternal, err.Error())}
		}
		return Response{ID: req.ID, OK: true, Result: map[string]any{"updated": true}}

	case MethodSystemIdentify:
		return Response{ID: req.ID, OK: true, Result: s.handler.Identify()}

	default:
		return Response{ID: req.ID, OK: false, Error: NewError(CodeBadRequest, "unknown method: "+req.Method)}
	}
}

// Dial is a small client helper used by roost-cli (and tests). Sends one
// request and returns the response. Connection is closed after.
func Dial(ctx context.Context, socketPath string, req Request) (Response, error) {
	d := net.Dialer{}
	conn, err := d.DialContext(ctx, "unix", socketPath)
	if err != nil {
		return Response{}, err
	}
	defer conn.Close()

	if err := json.NewEncoder(conn).Encode(req); err != nil {
		return Response{}, err
	}
	dec := json.NewDecoder(conn)
	var resp Response
	if err := dec.Decode(&resp); err != nil {
		return Response{}, err
	}
	return resp, nil
}
