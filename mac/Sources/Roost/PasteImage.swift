// Clipboard image-paste extraction for TerminalView.paste(_:).
//
// Three input shapes, in priority order:
//
//   1. File URLs already on the pasteboard (Finder copy of an image
//      file). The path is inserted verbatim — no temp copy. Multiple
//      paths are newline-joined so paths containing spaces survive a
//      multi-select.
//   2. PNG bytes on the pasteboard — passthrough to a temp file so
//      the agent always sees a `.png` extension.
//   3. Any other image representation we know how to read: TIFF,
//      JPEG, GIF, WebP, HEIC. The raw clipboard bytes are size-capped
//      first, then `CGImageSource` peeks dimensions before we let
//      ImageIO allocate a decode buffer.
//
// The encoded path is then pasted as ordinary bracketed-paste text via
// `TerminalView.paste(_:)`. Claude Code / Codex detect the `.png` path
// and offer to attach. No special image escape protocol is used; this
// mirrors the legacy Go implementation at `cmd/roost/paste_image.go`
// and cmux's `TerminalImageTransfer.swift`.

import AppKit
import Foundation
import ImageIO
import UniformTypeIdentifiers

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
    /// image whose RGBA buffer is 256 MiB — we check dimensions via
    /// `CGImageSource` (caching off) before letting ImageIO allocate
    /// the decode buffer. 40 MP comfortably covers 5K and 8K
    /// screenshots while rejecting obvious compression bombs.
    static let maxPixels = 40 * 1024 * 1024

    /// File extensions accepted in the file-URL fast path. The check is
    /// case-insensitive; the agent recognises any of these.
    static let imageExtensions: Set<String> = [
        "png", "jpg", "jpeg", "gif", "tiff", "tif", "webp", "heic", "heif", "bmp",
    ]

    /// Source-image pasteboard types we walk in priority order. PNG is
    /// first because it needs no re-encode round-trip; the rest are
    /// decoded by ImageIO and re-encoded to PNG. UTIs use raw strings
    /// (rather than `UTType.*.identifier`) so the table is stable
    /// across SDK changes and easy to compare against the Go port.
    static let sourceTypes: [(NSPasteboard.PasteboardType, Bool)] = [
        (.png, true),
        (.tiff, false),
        (NSPasteboard.PasteboardType("public.jpeg"), false),
        (NSPasteboard.PasteboardType("com.compuserve.gif"), false),
        (NSPasteboard.PasteboardType("org.webmproject.webp"), false),
        (NSPasteboard.PasteboardType("public.heic"), false),
        (NSPasteboard.PasteboardType("public.heif"), false),
    ]

    /// Top-level entry — try file URLs first (cheapest, no temp file),
    /// then fall back to materializing the clipboard image. Returns
    /// `.none` when there's nothing we know how to paste; callers
    /// should treat that as a no-op.
    static func extract(_ pb: NSPasteboard) -> Result {
        let urls = fileURLPaths(from: pb)
        if urls.count == 1 { return .path(urls[0]) }
        if urls.count > 1 { return .paths(urls) }
        if let path = materializeImage(from: pb) { return .path(path) }
        return .none
    }

    /// Walk the pasteboard for URL-shaped entries; return the subset
    /// that are local file URLs pointing at an image extension.
    /// `readObjects(forClasses:options:)` covers `.fileURL`, the
    /// legacy `NSFilenamesPboardType` indirection that Finder still
    /// emits on some drag/copy paths, and the raw `.fileURL` string
    /// form. URLs are de-duplicated by `standardizedFileURL` so a
    /// pasteboard that lists the same file under multiple types
    /// (Finder is fond of this) doesn't produce a duplicate.
    static func fileURLPaths(from pb: NSPasteboard) -> [String] {
        guard let urls = pb.readObjects(forClasses: [NSURL.self], options: nil) as? [URL] else {
            return []
        }
        var seen = Set<String>()
        var out: [String] = []
        for url in urls where url.isFileURL {
            let ext = url.pathExtension.lowercased()
            guard imageExtensions.contains(ext) else { continue }
            let path = url.standardizedFileURL.path
            if seen.insert(path).inserted {
                out.append(path)
            }
        }
        return out
    }

    /// Pull image bytes off the pasteboard, normalize to PNG, write to
    /// a temp file, return the absolute path. Returns nil on any
    /// failure (size cap, decode error, write error, no recognised
    /// representation present). The size cap is enforced against the
    /// raw clipboard bytes BEFORE any decode so a small compressed
    /// input can't force a large allocation.
    static func materializeImage(from pb: NSPasteboard) -> String? {
        let types = pb.types ?? []
        for (type, isPng) in sourceTypes {
            guard types.contains(type), let data = pb.data(forType: type) else { continue }
            guard data.count <= maxBytes else {
                RoostLogger.shared.warn(
                    "clipboard image: \(type.rawValue) exceeds \(maxBytes) bytes (\(data.count))"
                )
                return nil
            }
            if isPng {
                if let dims = pngDimensions(data),
                   !pixelCountFits(width: dims.width, height: dims.height)
                {
                    RoostLogger.shared.warn(
                        "clipboard image: png too large \(dims.width)x\(dims.height)"
                    )
                    return nil
                }
                return writeTempPng(data)
            }
            return reencodeToPng(sourceData: data, sourceType: type.rawValue)
        }
        return nil
    }

    /// Decode `data` via `CGImageSource` (caching off so the source
    /// bytes don't get retained twice) after a header-only dimension
    /// check, then re-encode to PNG via `CGImageDestination`. Caps are
    /// enforced on both the source byte count (by the caller) and the
    /// decoded pixel count (here) so neither stage can compression-bomb.
    private static func reencodeToPng(sourceData: Data, sourceType: String) -> String? {
        let opts = [
            kCGImageSourceShouldCache: false,
            kCGImageSourceShouldCacheImmediately: false,
        ] as CFDictionary
        guard let src = CGImageSourceCreateWithData(sourceData as CFData, opts) else {
            RoostLogger.shared.warn("clipboard image: \(sourceType) source-create failed")
            return nil
        }
        guard
            let props = CGImageSourceCopyPropertiesAtIndex(src, 0, opts)
                as? [CFString: Any],
            let w = props[kCGImagePropertyPixelWidth] as? Int,
            let h = props[kCGImagePropertyPixelHeight] as? Int
        else {
            RoostLogger.shared.warn("clipboard image: \(sourceType) props missing dimensions")
            return nil
        }
        guard pixelCountFits(width: w, height: h) else {
            RoostLogger.shared.warn(
                "clipboard image: \(sourceType) too large to decode \(w)x\(h)"
            )
            return nil
        }
        guard let cg = CGImageSourceCreateImageAtIndex(src, 0, opts) else {
            RoostLogger.shared.warn("clipboard image: \(sourceType) decode failed")
            return nil
        }
        let out = NSMutableData()
        guard
            let dest = CGImageDestinationCreateWithData(
                out,
                UTType.png.identifier as CFString,
                1,
                nil
            )
        else {
            RoostLogger.shared.warn("clipboard image: png destination-create failed")
            return nil
        }
        CGImageDestinationAddImage(dest, cg, nil)
        guard CGImageDestinationFinalize(dest) else {
            RoostLogger.shared.warn("clipboard image: png encode failed")
            return nil
        }
        let pngData = out as Data
        guard pngData.count <= maxBytes else {
            RoostLogger.shared.warn(
                "clipboard image: encoded png exceeds \(maxBytes) bytes (\(pngData.count))"
            )
            return nil
        }
        return writeTempPng(pngData)
    }

    /// Overflow-safe `width * height <= maxPixels`. Both `Int` and the
    /// trapping `*` would crash on huge values; the bitmap path's `&*`
    /// would silently wrap to a small product. Division-form check
    /// works whenever both dimensions are positive.
    static func pixelCountFits(width: Int, height: Int) -> Bool {
        guard width > 0, height > 0 else { return false }
        return width <= maxPixels / height
    }

    /// Peek a PNG IHDR for width/height without decoding pixels. PNG
    /// layout: 8-byte signature, then `length(4) + type(4) + payload
    /// + crc(4)`. The first chunk MUST be IHDR with length 13
    /// (RFC 2083 §11.2.2); we validate both the type bytes and the
    /// length before trusting the dimension fields, so a hand-crafted
    /// PNG-ish blob with garbage in the IHDR slot can't mislead the
    /// caller's bounds check.
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
        guard be32(8) == 13 else { return nil } // IHDR length is fixed
        let ihdr: [UInt8] = [0x49, 0x48, 0x44, 0x52] // "IHDR"
        for i in 0..<4 where data[data.startIndex + 12 + i] != ihdr[i] {
            return nil
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
