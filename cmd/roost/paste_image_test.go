package main

import (
	"bytes"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"os"
	"strings"
	"testing"
)

func makeRGBA(t *testing.T, w, h int) *image.RGBA {
	t.Helper()
	img := image.NewRGBA(image.Rect(0, 0, w, h))
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			img.Set(x, y, color.RGBA{R: uint8(x % 255), G: uint8(y % 255), B: 0xff, A: 0xff})
		}
	}
	return img
}

func encodePNG(t *testing.T, img image.Image) []byte {
	t.Helper()
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatalf("encode png: %v", err)
	}
	return buf.Bytes()
}

func encodeJPEG(t *testing.T, img image.Image) []byte {
	t.Helper()
	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, img, nil); err != nil {
		t.Fatalf("encode jpeg: %v", err)
	}
	return buf.Bytes()
}

func TestWriteClipboardImagePNGPassthrough(t *testing.T) {
	src := encodePNG(t, makeRGBA(t, 4, 3))

	path, err := writeClipboardImage(src, "image/png")
	if err != nil {
		t.Fatalf("writeClipboardImage: %v", err)
	}
	t.Cleanup(func() { _ = os.Remove(path) })

	if !strings.HasSuffix(path, ".png") {
		t.Errorf("expected .png suffix, got %q", path)
	}

	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read back: %v", err)
	}
	if !bytes.Equal(got, src) {
		t.Errorf("PNG bytes were re-encoded; want passthrough")
	}
}

func TestWriteClipboardImageJPEGReencode(t *testing.T) {
	src := encodeJPEG(t, makeRGBA(t, 8, 6))

	path, err := writeClipboardImage(src, "image/jpeg")
	if err != nil {
		t.Fatalf("writeClipboardImage: %v", err)
	}
	t.Cleanup(func() { _ = os.Remove(path) })

	f, err := os.Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer f.Close()
	cfg, format, err := image.DecodeConfig(f)
	if err != nil {
		t.Fatalf("decode config: %v", err)
	}
	if format != "png" {
		t.Errorf("output format = %q, want png", format)
	}
	if cfg.Width != 8 || cfg.Height != 6 {
		t.Errorf("dimensions = %dx%d, want 8x6", cfg.Width, cfg.Height)
	}
}

func TestWriteClipboardImageEmpty(t *testing.T) {
	if _, err := writeClipboardImage(nil, "image/png"); err == nil {
		t.Errorf("expected error for empty input")
	}
}

func TestWriteClipboardImageDecodeError(t *testing.T) {
	if _, err := writeClipboardImage([]byte("not actually an image"), "image/jpeg"); err == nil {
		t.Errorf("expected decode error for garbage input")
	}
}

func TestWriteClipboardImageUniquePaths(t *testing.T) {
	src := encodePNG(t, makeRGBA(t, 2, 2))
	a, err := writeClipboardImage(src, "image/png")
	if err != nil {
		t.Fatalf("writeClipboardImage: %v", err)
	}
	t.Cleanup(func() { _ = os.Remove(a) })
	b, err := writeClipboardImage(src, "image/png")
	if err != nil {
		t.Fatalf("writeClipboardImage: %v", err)
	}
	t.Cleanup(func() { _ = os.Remove(b) })
	if a == b {
		t.Errorf("expected unique paths, got %q twice", a)
	}
}
