// Renders testbed files that need drawing:
//   swift render.swift screenshot <out.png> <window title> <line>...   a 1440 × 900 app window with text
//   swift render.swift scan <out.pdf> <line>...                         a scanned page: an image, no text layer
import AppKit

let arguments = Array(CommandLine.arguments.dropFirst())
guard arguments.count >= 3 else { fatalError("usage: render.swift screenshot|scan <out> <text>...") }
let mode = arguments[0], output = URL(fileURLWithPath: arguments[1]), lines = Array(arguments[2...])

func bitmap(width: Int, height: Int, draw: () -> Void) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                               bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    draw()
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

func text(_ string: String, at point: NSPoint, size: CGFloat, weight: NSFont.Weight = .regular, color: NSColor = .black,
          font: NSFont? = nil) {
    let attributes: [NSAttributedString.Key: Any] = [.font: font ?? NSFont.systemFont(ofSize: size, weight: weight),
                                                     .foregroundColor: color]
    NSAttributedString(string: string, attributes: attributes).draw(at: point)
}

switch mode {
case "screenshot":
    let title = lines[0], body = Array(lines.dropFirst())
    let rep = bitmap(width: 1440, height: 900) {
        NSColor(white: 0.93, alpha: 1).setFill()
        NSRect(x: 0, y: 0, width: 1440, height: 900).fill()
        // Window with a title bar and traffic lights.
        let window = NSRect(x: 120, y: 80, width: 1200, height: 740)
        NSColor.white.setFill()
        NSBezierPath(roundedRect: window, xRadius: 12, yRadius: 12).fill()
        NSColor(white: 0.97, alpha: 1).setFill()
        NSRect(x: 120, y: 772, width: 1200, height: 48).fill()
        for (i, color) in [NSColor.systemRed, .systemYellow, .systemGreen].enumerated() {
            color.setFill()
            NSBezierPath(ovalIn: NSRect(x: 140 + i * 24, y: 788, width: 14, height: 14)).fill()
        }
        text(title, at: NSPoint(x: 660, y: 786), size: 15, weight: .semibold, color: .darkGray)
        var y: CGFloat = 700
        for (i, line) in body.enumerated() {
            let monospaced = line.hasPrefix("$ ") || line.hasPrefix("  File") || line.contains("Error:")
            let font = monospaced ? NSFont.monospacedSystemFont(ofSize: 20, weight: .regular) : nil
            text(line, at: NSPoint(x: 180, y: y), size: i == 0 ? 30 : 22, weight: i == 0 ? .bold : .regular,
                 color: line.contains("declined") || line.contains("Error") ? .systemRed : .black, font: font)
            y -= i == 0 ? 64 : 40
        }
    }
    try! rep.representation(using: .png, properties: [:])!.write(to: output)

case "scan":
    // A4 at 150 dpi, slightly grey paper with a little noise, text a little blurred: as a cheap scanner makes it.
    let rep = bitmap(width: 1240, height: 1754) {
        NSColor(white: 0.96, alpha: 1).setFill()
        NSRect(x: 0, y: 0, width: 1240, height: 1754).fill()
        var generator = SystemRandomNumberGenerator()
        for _ in 0..<40000 {
            NSColor(white: CGFloat.random(in: 0.85...0.95, using: &generator), alpha: 0.5).setFill()
            NSRect(x: Int.random(in: 0..<1240, using: &generator), y: Int.random(in: 0..<1754, using: &generator),
                   width: 2, height: 2).fill()
        }
        var y: CGFloat = 1560
        for (i, line) in lines.enumerated() {
            text(line, at: NSPoint(x: 120, y: y), size: i == 0 ? 40 : 26, weight: i == 0 ? .bold : .regular,
                 color: NSColor(white: 0.12, alpha: 1), font: i == 0 ? nil : NSFont(name: "Times New Roman", size: 26))
            y -= i == 0 ? 90 : 46
        }
    }
    let image = rep.cgImage!
    var box = CGRect(x: 0, y: 0, width: 595, height: 842)
    let context = CGContext(output as CFURL, mediaBox: &box, nil)!
    context.beginPDFPage(nil)
    context.draw(image, in: box)
    context.endPDFPage()
    context.closePDF()

default:
    fatalError("unknown mode \(mode)")
}
