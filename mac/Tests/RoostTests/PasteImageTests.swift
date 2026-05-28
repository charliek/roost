// Clipboard image-paste extraction tests, Swift companion to the GTK
// suite in `crates/roost-linux/src/paste_image.rs::tests`. Covers the
// three input shapes `TerminalView.paste(_:)` falls back through:
// file URLs, PNG passthrough, and ImageIO-driven re-encode.
//
// Each test creates an isolated `NSPasteboard(name:)` so the host
// machine's `NSPasteboard.general` is untouched. The fixture builders
// touch `NSGraphicsContext.current`, which is process-global; tests
// that call them are `@MainActor`-isolated so swift-testing won't
// fan them out across threads.

import AppKit
import Foundation
import Testing

@testable import Roost

private func freshPasteboard() -> NSPasteboard {
    NSPasteboard(name: NSPasteboard.Name("ai.stridelabs.Roost.test-\(UUID().uuidString)"))
}

@MainActor
private func makeBitmapRep(width: Int = 8, height: Int = 8) -> NSBitmapImageRep {
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: width,
        pixelsHigh: height,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0,
        bitsPerPixel: 0
    ) else {
        fatalError("NSBitmapImageRep init failed")
    }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSColor.red.set()
    NSBezierPath(rect: NSRect(x: 0, y: 0, width: width, height: height)).fill()
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

@MainActor
private func makePNG(width: Int = 8, height: Int = 8) -> Data {
    guard let data = makeBitmapRep(width: width, height: height)
        .representation(using: .png, properties: [:])
    else {
        fatalError("PNG encode failed")
    }
    return data
}

@MainActor
private func makeTIFF(width: Int = 8, height: Int = 8) -> Data {
    guard let data = makeBitmapRep(width: width, height: height).tiffRepresentation else {
        fatalError("TIFF encode failed")
    }
    return data
}

@MainActor
private func makeJPEG(width: Int = 8, height: Int = 8) -> Data {
    guard let data = makeBitmapRep(width: width, height: height)
        .representation(using: .jpeg, properties: [:])
    else {
        fatalError("JPEG encode failed")
    }
    return data
}

private func removeIfExists(_ path: String) {
    try? FileManager.default.removeItem(atPath: path)
}

@Test
func pasteImage_emptyPasteboardReturnsNone() {
    let pb = freshPasteboard()
    #expect(PasteImage.extract(pb) == .none)
}

@Test @MainActor
func pasteImage_pngPassthroughWritesTempFileWithSameBytes() throws {
    let pb = freshPasteboard()
    pb.clearContents()
    let png = makePNG()
    pb.setData(png, forType: .png)

    let path = try #require(PasteImage.materializeImage(from: pb))
    defer { removeIfExists(path) }

    let url = URL(fileURLWithPath: path)
    #expect(url.lastPathComponent.hasPrefix("roost-image-"))
    #expect(url.pathExtension == "png")
    let written = try Data(contentsOf: url)
    // PNG passthrough preserves the exact bytes — no AppKit re-encode.
    #expect(written == png)
}

@Test @MainActor
func pasteImage_tiffIsReencodedToPng() throws {
    let pb = freshPasteboard()
    pb.clearContents()
    pb.setData(makeTIFF(), forType: .tiff)

    let path = try #require(PasteImage.materializeImage(from: pb))
    defer { removeIfExists(path) }

    let url = URL(fileURLWithPath: path)
    #expect(url.pathExtension == "png")
    // First 8 bytes of any PNG are the fixed signature.
    let data = try Data(contentsOf: url)
    let sig: [UInt8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    #expect(Array(data.prefix(8)) == sig)
}

@Test @MainActor
func pasteImage_jpegIsReencodedToPng() throws {
    let pb = freshPasteboard()
    pb.clearContents()
    pb.setData(makeJPEG(), forType: NSPasteboard.PasteboardType("public.jpeg"))

    let path = try #require(PasteImage.materializeImage(from: pb))
    defer { removeIfExists(path) }
    let data = try Data(contentsOf: URL(fileURLWithPath: path))
    let sig: [UInt8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    #expect(Array(data.prefix(8)) == sig)
}

@Test
func pasteImage_oversizedPngIsRejected() {
    // Synthesize >maxBytes of "PNG-shaped" bytes — we only need the
    // signature + length check; materializeImage trips the size cap
    // before pngDimensions ever runs.
    let pb = freshPasteboard()
    pb.clearContents()
    var data = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
    data.append(Data(count: PasteImage.maxBytes + 1 - data.count))
    pb.setData(data, forType: .png)

    #expect(PasteImage.materializeImage(from: pb) == nil)
}

@Test
func pasteImage_oversizedJpegIsRejectedBeforeDecode() {
    // Stuff >maxBytes of arbitrary bytes under public.jpeg. The size
    // cap is the first gate; we never call CGImageSource on the blob.
    let pb = freshPasteboard()
    pb.clearContents()
    let huge = Data(count: PasteImage.maxBytes + 1)
    pb.setData(huge, forType: NSPasteboard.PasteboardType("public.jpeg"))
    #expect(PasteImage.materializeImage(from: pb) == nil)
}

@Test
func pasteImage_extractFileURL_returnsPath() throws {
    let temp = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("paste-image-fixture-\(UUID().uuidString).png")
    try Data().write(to: URL(fileURLWithPath: temp))
    defer { removeIfExists(temp) }

    let pb = freshPasteboard()
    pb.clearContents()
    pb.declareTypes([.fileURL], owner: nil)
    let ok = pb.writeObjects([URL(fileURLWithPath: temp) as NSURL])
    #expect(ok)

    #expect(PasteImage.extract(pb) == .path(temp))
}

@Test
func pasteImage_extractFileURL_skipsNonImageExtensions() throws {
    let temp = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("paste-image-fixture-\(UUID().uuidString).txt")
    try Data().write(to: URL(fileURLWithPath: temp))
    defer { removeIfExists(temp) }

    let pb = freshPasteboard()
    pb.clearContents()
    pb.declareTypes([.fileURL], owner: nil)
    _ = pb.writeObjects([URL(fileURLWithPath: temp) as NSURL])

    // No image bytes either → none. .txt is not an image extension.
    #expect(PasteImage.extract(pb) == .none)
}

@Test
func pasteImage_extractMultipleFileURLs_joinsAsPaths() throws {
    let a = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("paste-image-fixture-\(UUID().uuidString).png")
    let b = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("paste-image-fixture-\(UUID().uuidString).jpg")
    try Data().write(to: URL(fileURLWithPath: a))
    try Data().write(to: URL(fileURLWithPath: b))
    defer {
        removeIfExists(a)
        removeIfExists(b)
    }

    let pb = freshPasteboard()
    pb.clearContents()
    pb.declareTypes([.fileURL], owner: nil)
    _ = pb.writeObjects([URL(fileURLWithPath: a) as NSURL, URL(fileURLWithPath: b) as NSURL])

    if case .paths(let ps) = PasteImage.extract(pb) {
        #expect(Set(ps) == Set([a, b]))
    } else {
        Issue.record("expected .paths, got \(PasteImage.extract(pb))")
    }
}

@Test
func pasteImage_extractFileURL_acceptsLegacyFilenamesEntry() throws {
    // Some Finder drag/copy paths still emit the legacy
    // `NSFilenamesPboardType` (an `NSArray<NSString>` plist) instead
    // of the modern `.fileURL` type. `readObjects(forClasses:[NSURL])`
    // is documented to expand that path indirection — verify the
    // behavior so the fallback doesn't regress.
    let temp = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("paste-image-fixture-\(UUID().uuidString).png")
    try Data().write(to: URL(fileURLWithPath: temp))
    defer { removeIfExists(temp) }

    let pb = freshPasteboard()
    pb.clearContents()
    let legacy = NSPasteboard.PasteboardType("NSFilenamesPboardType")
    pb.declareTypes([legacy], owner: nil)
    let plist = try PropertyListSerialization.data(
        fromPropertyList: [temp],
        format: .xml,
        options: 0
    )
    let wrote = pb.setData(plist, forType: legacy)
    // The legacy type is deprecated, so AppKit may refuse the write
    // under modern SDKs. Treat refusal as "skip the assertion" so the
    // test stays portable across macOS versions, but assert when the
    // write succeeds — that's the path we care about regressing.
    if wrote {
        #expect(PasteImage.fileURLPaths(from: pb).contains(temp))
    }
}

@Test
func pasteImage_pngDimensions_parsesIHDR() throws {
    // Hand-roll a minimal valid PNG header so we don't depend on the
    // graphics-context bitmap path here (which can't run off-main).
    var png = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x0D]) // IHDR length = 13
    png.append(contentsOf: [0x49, 0x48, 0x44, 0x52]) // "IHDR"
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x0C]) // width = 12
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x07]) // height = 7
    let dims = try #require(PasteImage.pngDimensions(png))
    #expect(dims.width == 12)
    #expect(dims.height == 7)
}

@Test
func pasteImage_pngDimensions_rejectsBadIhdrType() {
    // PNG signature + chunk length = 13, but the chunk type is "FOOO"
    // instead of "IHDR" — pngDimensions must reject so the caller's
    // bounds check isn't fooled by garbage in the dimension slot.
    var png = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x0D])
    png.append(contentsOf: [0x46, 0x4F, 0x4F, 0x4F])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x0C])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x07])
    #expect(PasteImage.pngDimensions(png) == nil)
}

@Test
func pasteImage_pngDimensions_rejectsBadIhdrLength() {
    // Signature OK, chunk type "IHDR" OK, but length lies about its
    // size (claims 99 bytes). Reject defensively.
    var png = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x63]) // length = 99 (invalid)
    png.append(contentsOf: [0x49, 0x48, 0x44, 0x52])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x0C])
    png.append(contentsOf: [0x00, 0x00, 0x00, 0x07])
    #expect(PasteImage.pngDimensions(png) == nil)
}

@Test
func pasteImage_pngDimensions_rejectsNonPng() {
    #expect(PasteImage.pngDimensions(Data([0xff, 0xd8, 0xff, 0xe0])) == nil) // jpeg-ish
    #expect(PasteImage.pngDimensions(Data()) == nil)
}

@Test
func pasteImage_pixelCountFits_bounds() {
    let max = PasteImage.maxPixels
    #expect(PasteImage.pixelCountFits(width: 1024, height: 1024))
    #expect(PasteImage.pixelCountFits(width: 1, height: max))
    #expect(PasteImage.pixelCountFits(width: max, height: 1))
    #expect(!PasteImage.pixelCountFits(width: 1, height: max + 1))
    // 100_000 × 100_000 = 10^10 — exceeds `max`; the division-form
    // check rejects without ever materializing the product.
    #expect(!PasteImage.pixelCountFits(width: 100_000, height: 100_000))
    #expect(!PasteImage.pixelCountFits(width: 0, height: 1))
    #expect(!PasteImage.pixelCountFits(width: 1, height: 0))
    #expect(!PasteImage.pixelCountFits(width: -1, height: 1))
}
