package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"image"
	_ "image/gif"
	_ "image/jpeg"
	"image/png"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"time"

	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gio/v2"

	"github.com/charliek/roost/internal/ghostty"
)

// pasteImageMaxBytes caps the clipboard image we'll materialize. Matches
// cmux's 10 MiB ceiling and keeps the in-process decode bounded.
const pasteImageMaxBytes = 10 * 1024 * 1024

// pasteImageMaxPixels guards the re-encode path against compression-bomb
// inputs: a 10 MiB JPEG can describe an 8000x8000 image whose decoded
// RGBA buffer is 256 MiB. We check dimensions via image.DecodeConfig
// before allocating the full buffer. 40 megapixels comfortably covers
// 5K (14.7 MP) and 8K (33 MP) screenshots while rejecting the obvious
// adversarial cases.
const pasteImageMaxPixels = 40 * 1024 * 1024

// clipboardImageMimes is the priority order we use when negotiating an
// image format with the clipboard. PNG is first because it needs no
// re-encoding; the rest are stdlib-decodable and re-encoded to PNG
// before being written to disk.
var clipboardImageMimes = []string{
	"image/png",
	"image/jpeg",
	"image/gif",
}

// firstAvailableImageMime returns the first MIME from clipboardImageMimes
// that the clipboard actually advertises, or "" if none match.
func firstAvailableImageMime(formats *gdk.ContentFormats) string {
	if formats == nil {
		return ""
	}
	for _, m := range clipboardImageMimes {
		if formats.ContainMIMEType(m) {
			return m
		}
	}
	return ""
}

// pasteImageFromClipboard reads an image off the clipboard, writes it
// to a temp PNG, and pastes the file path through the same encode +
// QueueWrite pipeline as a text paste. Claude Code / Codex / similar
// agents detect the pasted path and offer to attach the image.
//
// We capture bracketed-paste state on the main thread before handing
// the heavy work to a goroutine — libghostty-vt terminal handles must
// not be touched off-main (see CLAUDE.md threading rules).
func (a *App) pasteImageFromClipboard(sess *Session, clip *gdk.Clipboard, mime string) {
	bracketed := sess.term.BracketedPasteEnabled()
	clip.ReadAsync(context.Background(), []string{mime}, 0, func(res gio.AsyncResulter) {
		gotMime, streamer, err := clip.ReadFinish(res)
		if err != nil {
			slog.Warn("clipboard image read", "err", err)
			return
		}
		if streamer == nil {
			slog.Warn("clipboard image read", "err", "no stream returned")
			return
		}
		stream := gio.BaseInputStream(streamer)
		go finishImagePaste(sess, bracketed, stream, gotMime)
	})
}

func finishImagePaste(sess *Session, bracketed bool, stream *gio.InputStream, mime string) {
	defer func() {
		if err := stream.Close(context.Background()); err != nil {
			slog.Warn("clipboard image stream close", "err", err)
		}
	}()

	data, err := readClipboardStream(stream, pasteImageMaxBytes)
	if err != nil {
		slog.Warn("clipboard image read", "err", err)
		return
	}
	path, err := writeClipboardImage(data, mime)
	if err != nil {
		slog.Warn("clipboard image write", "err", err)
		return
	}
	encoded, err := ghostty.EncodePaste([]byte(path), bracketed)
	if err != nil {
		slog.Warn("paste encode", "err", err)
		return
	}
	sess.QueueWrite(encoded)
}

// readClipboardStream drains a GInputStream up to maxBytes. Returns an
// error if the payload exceeds the cap so we don't silently truncate a
// large image into an unusable file.
func readClipboardStream(stream *gio.InputStream, maxBytes int) ([]byte, error) {
	ctx := context.Background()
	out := make([]byte, 0, 64*1024)
	buf := make([]byte, 64*1024)
	for {
		n, err := stream.Read(ctx, buf)
		if n > 0 {
			if len(out)+n > maxBytes {
				return nil, fmt.Errorf("clipboard image exceeds %d bytes", maxBytes)
			}
			out = append(out, buf[:n]...)
		}
		if err != nil {
			if err == io.EOF {
				break
			}
			return nil, err
		}
		if n == 0 {
			break
		}
	}
	return out, nil
}

// writeClipboardImage materializes the clipboard bytes as a PNG in
// os.TempDir() and returns the path. Non-PNG inputs are decoded and
// re-encoded so the agent always sees a `.png`. The filename uses a
// nanosecond timestamp and 8 random hex bytes — no spaces or shell
// metacharacters, so the path can be pasted as-is into either a shell
// prompt or an agent's input box.
func writeClipboardImage(data []byte, mime string) (string, error) {
	if len(data) == 0 {
		return "", fmt.Errorf("clipboard image is empty")
	}
	var pngBytes []byte
	if mime == "image/png" {
		pngBytes = data
	} else {
		cfg, _, err := image.DecodeConfig(bytes.NewReader(data))
		if err != nil {
			return "", fmt.Errorf("decode %s config: %w", mime, err)
		}
		if int64(cfg.Width)*int64(cfg.Height) > pasteImageMaxPixels {
			return "", fmt.Errorf("clipboard image too large to decode: %dx%d", cfg.Width, cfg.Height)
		}
		img, _, err := image.Decode(bytes.NewReader(data))
		if err != nil {
			return "", fmt.Errorf("decode %s: %w", mime, err)
		}
		var buf bytes.Buffer
		if err := png.Encode(&buf, img); err != nil {
			return "", fmt.Errorf("encode png: %w", err)
		}
		pngBytes = buf.Bytes()
	}

	var rnd [8]byte
	if _, err := rand.Read(rnd[:]); err != nil {
		return "", err
	}
	name := fmt.Sprintf("roost-image-%d-%s.png", time.Now().UnixNano(), hex.EncodeToString(rnd[:]))
	path := filepath.Join(os.TempDir(), name)
	if err := os.WriteFile(path, pngBytes, 0o600); err != nil {
		return "", err
	}
	return path, nil
}
