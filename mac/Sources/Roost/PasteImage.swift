// Clipboard image-paste extraction for TerminalView.paste(_:).
//
// Three input shapes, in priority order:
//
//   1. File URLs already on the pasteboard (Finder copy of an image
//      file). The path is inserted verbatim — no temp copy.
//   2. PNG bytes on the pasteboard — passthrough to a temp file so
//      the agent always sees a `.png` extension.
//   3. Any other image representation libghostty-vt's host can render
//      via `NSImage(pasteboard:)` (TIFF, JPEG, GIF, WebP, HEIC, …).
//      Decoded once and re-encoded to PNG.
//
// The encoded path is then pasted as ordinary bracketed-paste text via
// `TerminalView.paste(_:)`. Claude Code / Codex detect the `.png` path
// and offer to attach. No special image escape protocol is used; this
// mirrors the legacy Go implementation at `cmd/roost/paste_image.go`
// and cmux's `TerminalImageTransfer.swift`.

import AppKit
import Foundation

enum PasteImage {
    enum Result: Equatable {
        case path(String)
        case paths([String])
        case none
    }

    /// Maximum clipboard payload we'll materialize. Matches the legacy
    /// Go cap (cmd/roost/paste_image.go:27) and cmux's 10 MiB ceiling.
    static let maxBytes = 10 * 1024 * 1024

    /// Decoded-megapixel cap. A 10 MiB JPEG can describe an 8000×8000
    /// image whose RGBA buffer is 256 MiB — we check dimensions before
    /// re-encoding so a compression-bomb input can't OOM the renderer.
    /// 40 MP comfortably covers 5K and 8K screenshots.
    static let maxPixels = 40 * 1024 * 1024

    /// File extensions accepted in the file-URL fast path. The check is
    /// case-insensitive; the agent recognises any of these.
    static let imageExtensions: Set<String> = [
        "png", "jpg", "jpeg", "gif", "tiff", "tif", "webp", "heic", "heif", "bmp",
    ]

    /// Top-level entry — try file URLs first (cheapest, no temp file),
    /// then fall back to materializing the clipboard image. Returns
    /// `.none` when there's nothing we know how to paste; callers
    /// should treat that as a no-op (the keystroke is already
    /// consumed elsewhere).
    static func extract(_ pb: NSPasteboard) -> Result {
        let urls = fileURLPaths(from: pb)
        if urls.count == 1 { return .path(urls[0]) }
        if urls.count > 1 { return .paths(urls) }
        if let path = materializeImage(from: pb) { return .path(path) }
        return .none
    }

    /// Walk the pasteboard for `NSURL`-shaped entries; return the
    /// subset that are local file URLs pointing at an image
    /// extension. `readObjects(forClasses:options:)` covers both the
    /// modern `.fileURL` UTI and the legacy `NSFilenamesPboardType`
    /// indirection that Finder still emits on some drag/copy paths.
    static func fileURLPaths(from pb: NSPasteboard) -> [String] {
        guard let urls = pb.readObjects(forClasses: [NSURL.self], options: nil) as? [URL] else {
            return []
        }
        return urls.compactMap { url -> String? in
            guard url.isFileURL else { return nil }
            let ext = url.pathExtension.lowercased()
            guard imageExtensions.contains(ext) else { return nil }
            return url.standardizedFileURL.path
        }
    }

    /// Pull image bytes off the pasteboard, normalize to PNG, write to
    /// a temp file, return the absolute path. Returns nil on any
    /// failure (size cap, decode error, write error, no image
    /// representation present).
    static func materializeImage(from pb: NSPasteboard) -> String? {
        let types = pb.types ?? []

        // PNG passthrough. Skip the re-encode round-trip — preserves
        // ICC profile + avoids any AppKit-driven colorspace drift.
        if types.contains(.png), let data = pb.data(forType: .png) {
            guard data.count <= maxBytes else {
                RoostLogger.shared.warn(
                    "clipboard image: png exceeds \(maxBytes) bytes (\(data.count))"
                )
                return nil
            }
            if let dims = pngDimensions(data), dims.width * dims.height > maxPixels {
                RoostLogger.shared.warn(
                    "clipboard image: png too large \(dims.width)x\(dims.height)"
                )
                return nil
            }
            return writeTempPng(data)
        }

        // Fallback: ask AppKit to decode whatever the pasteboard has.
        // `NSImage(pasteboard:)` walks the standard image UTIs in
        // order; `tiffRepresentation` then gives us a uniform handle.
        guard let img = NSImage(pasteboard: pb) else { return nil }
        guard let tiff = img.tiffRepresentation else { return nil }
        guard tiff.count <= maxBytes else {
            RoostLogger.shared.warn(
                "clipboard image: decoded tiff exceeds \(maxBytes) bytes (\(tiff.count))"
            )
            return nil
        }
        guard let rep = NSBitmapImageRep(data: tiff) else { return nil }
        let pixels = rep.pixelsWide &* rep.pixelsHigh
        guard pixels <= maxPixels else {
            RoostLogger.shared.warn(
                "clipboard image: too large to decode \(rep.pixelsWide)x\(rep.pixelsHigh)"
            )
            return nil
        }
        guard let encoded = rep.representation(using: .png, properties: [:]) else {
            RoostLogger.shared.warn("clipboard image: png re-encode returned nil")
            return nil
        }
        return writeTempPng(encoded)
    }

    /// Peek a PNG IHDR for width/height without decoding pixels. PNG
    /// layout: 8-byte signature, then a length-prefixed `IHDR` chunk
    /// whose first 8 payload bytes are width(4) + height(4) BE.
    static func pngDimensions(_ data: Data) -> (width: Int, height: Int)? {
        guard data.count >= 24 else { return nil }
        let sig: [UInt8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        for i in 0..<8 where data[data.startIndex + i] != sig[i] {
            return nil
        }
        func be32(_ at: Int) -> Int {
            let base = data.startIndex + at
            return (Int(data[base]) << 24)
                | (Int(data[base + 1]) << 16)
                | (Int(data[base + 2]) << 8)
                | Int(data[base + 3])
        }
        return (be32(16), be32(20))
    }

    private static func writeTempPng(_ data: Data) -> String? {
        var rnd = [UInt8](repeating: 0, count: 8)
        let status = SecRandomCopyBytes(kSecRandomDefault, rnd.count, &rnd)
        guard status == errSecSuccess else {
            RoostLogger.shared.warn("clipboard image: SecRandomCopyBytes failed (\(status))")
            return nil
        }
        let hex = rnd.map { String(format: "%02x", $0) }.joined()
        let nanos = UInt64(Date().timeIntervalSince1970 * 1_000_000_000)
        let name = "roost-image-\(nanos)-\(hex).png"
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent(name)
        do {
            try data.write(to: url, options: .atomic)
            return url.path
        } catch {
            RoostLogger.shared.warn("clipboard image: write \(url.path) failed: \(error)")
            return nil
        }
    }
}
