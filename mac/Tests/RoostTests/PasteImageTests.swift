// Clipboard image-paste extraction tests, Swift companion to the GTK
// suite in `crates/roost-linux/tests/paste_image_test.rs`. Covers the
// three input shapes `TerminalView.paste(_:)` falls back through:
// file URLs, PNG passthrough, and AppKit-driven re-encode.
//
// Each test creates an isolated `NSPasteboard(name:)` so the host
// machine's `NSPasteboard.general` is untouched and the tests are safe
// to run in parallel.

import AppKit
import Foundation
import Testing

@testable import Roost

private func freshPasteboard() -> NSPasteboard {
    NSPasteboard(name: NSPasteboard.Name("ai.stridelabs.Roost.test-\(UUID().uuidString)"))
}

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

private func makePNG(width: Int = 8, height: Int = 8) -> Data {
    guard let data = makeBitmapRep(width: width, height: height)
        .representation(using: .png, properties: [:])
    else {
        fatalError("PNG encode failed")
    }
    return data
}

private func makeTIFF(width: Int = 8, height: Int = 8) -> Data {
    guard let data = makeBitmapRep(width: width, height: height).tiffRepresentation else {
        fatalError("TIFF encode failed")
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

@Test
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

@Test
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
func pasteImage_extractFileURL_returnsPath() throws {
    let temp = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("paste-image-fixture-\(UUID().uuidString).png")
    try Data().write(to: URL(fileURLWithPath: temp))
    defer { removeIfExists(temp) }

    let pb = freshPasteboard()
    pb.clearContents()
    // pb.declareTypes(...) + writeObjects is the supported way to put
    // a file URL on a non-general pasteboard.
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
func pasteImage_pngDimensions_parsesIHDR() {
    let png = makePNG(width: 12, height: 7)
    let dims = PasteImage.pngDimensions(png)
    #expect(dims?.width == 12)
    #expect(dims?.height == 7)
}

@Test
func pasteImage_pngDimensions_rejectsNonPng() {
    #expect(PasteImage.pngDimensions(Data([0xff, 0xd8, 0xff, 0xe0])) == nil) // jpeg-ish
    #expect(PasteImage.pngDimensions(Data()) == nil)
}
