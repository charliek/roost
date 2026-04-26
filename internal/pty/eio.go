package pty

import "syscall"

// syscallEIO returns the EIO errno value for the current platform. On
// Linux, reading from a master pty whose slave has been closed returns
// EIO; on Darwin it returns 0 (EOF) cleanly. We compare against this
// to translate Linux's quirk into io.EOF for callers.
func syscallEIO() error { return syscall.EIO }
