// Package ghostty wraps libghostty-vt via cgo.
//
// This is the only package in Roost permitted to use cgo. Everything else
// — the renderer, PTY, OSC routing, UI — is pure Go and goes through this
// package's exported Go API.
//
// The bindings are deliberately narrow: we expose only what Roost uses.
// The C symbols come from build/out/include/ghostty/vt.h, produced by
// `./build/build.sh libghostty`.
package ghostty
