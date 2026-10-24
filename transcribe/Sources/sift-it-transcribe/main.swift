// `sift-it-transcribe <model folder>`: speech to text for sift-it's indexer, in a process of its own. Parakeet Ultra
// runs on the Neural Engine through FluidAudio (Core ML), which is Swift; a process of its own also gives back all of
// its memory, the Neural Engine's copy of the model too, when the indexer has nothing more to write down and closes
// its stdin.
//
// It loads the model, says so on stdout, then reads chunks of sound from stdin until it closes: a little-endian u32
// sample count, then that many little-endian f32 samples (16 kHz mono, -1 to 1). For each chunk it writes one JSON
// line on stdout: the words it heard, with their punctuation and their times in seconds from the chunk's start
// (sift-it cuts them into passages).
//
//   {"event":"ready","model":"parakeet-ultra","backend":"ane"}
//   {"words":[{"start":5.6,"end":5.84,"text":"This"},{"start":5.84,"end":6.08,"text":"is"},…]}
//   {"error":"…"}                         (that chunk failed; the next one is read as usual)

import FluidAudio
import Foundation

struct Word: Encodable {
    let start: Double
    let end: Double
    let text: String

    /// To the hundredth of a second: the model's steps are 80 ms.
    init(_ word: WordTiming) {
        start = (word.startTime * 100).rounded() / 100
        end = (word.endTime * 100).rounded() / 100
        text = word.word
    }
}

// Standard input and output through read(2) and write(2), into and out of Swift's own buffers: FileHandle's NSData
// would be autoreleased, and the main thread's pool never drains (a chunk's 11 MB stayed, now and then).

func say(_ value: some Encodable) {
    var line = (try? JSONEncoder().encode(value)) ?? Data(#"{"error":"can't say it"}"#.utf8)
    line.append(UInt8(ascii: "\n"))
    line.withUnsafeBytes { bytes in
        var done = 0
        while done < bytes.count {
            let wrote = write(STDOUT_FILENO, bytes.baseAddress! + done, bytes.count - done)
            guard wrote > 0 else { return }
            done += wrote
        }
    }
}

/// Fills `buffer` from stdin: false once it's closed.
func read(into buffer: UnsafeMutableRawBufferPointer) -> Bool {
    var done = 0
    while done < buffer.count {
        let read = Darwin.read(STDIN_FILENO, buffer.baseAddress! + done, buffer.count - done)
        guard read > 0 else { return false }
        done += read
    }
    return true
}

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: sift-it-transcribe <model folder>\n".utf8))
    exit(2)
}
// What's said stays out of the system log.
AppLogger.minimumLevel = .warning

let manager: AsrManager
let layers: Int
do {
    let version = AsrModelVersion.ultra
    let models = try AsrModels.loadLocal(from: URL(fileURLWithPath: CommandLine.arguments[1]), version: version)
    // FluidAudio decodes a chunk in 15-second windows. Each starts with the end of the one before (an 80 ms mel
    // prefix) rather than at a silence nearby: on Dutch conversation that makes a third fewer errors, as fast.
    let config = ASRConfig(
        tdtConfig: TdtConfig(blankId: version.blankId), encoderHiddenSize: version.encoderHiddenSize,
        melChunkContext: true)
    manager = AsrManager(config: config)
    try await manager.loadModels(models)
    layers = await manager.decoderLayerCount
    // The first call prepares the rest: pay for it now.
    var state = TdtDecoderState.make(decoderLayers: layers)
    _ = try await manager.transcribe([Float](repeating: 0, count: 16_000), decoderState: &state)
} catch {
    say(["error": "can't load the model: \(error)"])
    exit(1)
}
say(["event": "ready", "model": "parakeet-ultra", "backend": "ane"])

// Apple silicon is little-endian, as the protocol is: the samples are read as they come.
var count: UInt32 = 0
while withUnsafeMutableBytes(of: &count, read(into:)) {
    var samples = [Float](repeating: 0, count: Int(count))
    guard samples.withUnsafeMutableBytes(read(into:)) else { break }
    if samples.isEmpty {
        say(["words": [Word]()])
        continue
    }
    do {
        var state = TdtDecoderState.make(decoderLayers: layers)
        let result = try await manager.transcribe(samples, decoderState: &state)
        say(["words": buildWordTimings(from: result.tokenTimings ?? []).map(Word.init)])
    } catch {
        say(["error": "\(error)"])
    }
}
