// `fluid-transcribe <folder with Parakeet Ultra's .mlmodelc files>`: sift-it-transcribe's protocol on FluidAudio.
//
// It loads the models, says so on stdout, then reads chunks of sound from stdin until it closes: a little-endian u32
// sample count, then that many little-endian f32 samples (16 kHz mono). For each chunk it writes one JSON line:
//   {"event":"ready","model":"parakeet-ultra","backend":"coreml-ane","load_seconds":…}
//   {"segments":[{"start":0,"end":…,"text":"…"}],"seconds":…}
// FLUID_UNITS=ane|gpu|cpu picks where the encoder runs (ane by default; the decoder and joint stay on the ANE).
//
// Experiment, how long chunks are decoded (FluidAudio cuts them into 15 s windows with 2 s overlaps and merges them):
//   FLUID_MEL_CONTEXT=0|1     ASRConfig.melChunkContext (unset: off for Ultra, so windows start at silences)
//   FLUID_DUAL_DECODE=1       ASRConfig.dualDecodeArbitration (probe three seam strategies, keep one)
//   FLUID_SEAM_REPAIR=0       no post-merge seam-gap repair; FLUID_SEAM_MIN_GAP=<s> its threshold (1.5 s)
//   FLUID_PAUSE_CUT=1         no overlaps at all: cut each chunk at pauses into windows of at most 15 s, decode each
//                             alone and join the text (FluidAudio's own late-pause rule, see pauseCuts)
import CoreML
import FluidAudio
import Foundation

func say(_ object: [String: Any]) {
    let data = try! JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
}

func readExactly(_ count: Int) -> Data? {
    var data = Data()
    while data.count < count {
        guard let more = try? FileHandle.standardInput.read(upToCount: count - data.count), !more.isEmpty else {
            return nil
        }
        data.append(more)
    }
    return data
}

let environment = ProcessInfo.processInfo.environment

/// Where to cut a chunk into windows of at most 15 s with no overlap: in each window's last 5 s, the middle of the
/// latest run of at least 3 quiet 80 ms frames (RMS under 0.18 × the window's loudest frame, at least 0.004), as
/// FluidAudio's `latestPauseCut` does for its ladder encoders. Without such a run, the quietest frame there.
func pauseCuts(_ samples: [Float]) -> [Range<Int>] {
    let frame = 1_280, window = 240_000, tail = 5 * 16_000
    var ranges: [Range<Int>] = []
    var start = 0
    while samples.count - start > window {
        let end = start + window
        func rms(_ at: Int) -> Float {
            var sum: Float = 0
            for i in at..<min(at + frame, samples.count) { sum += samples[i] * samples[i] }
            return (sum / Float(frame)).squareRoot()
        }
        let frames = stride(from: start, to: end - frame + 1, by: frame).map { ($0, rms($0)) }
        let peak = frames.map(\.1).max() ?? 0
        let quiet = max(0.18 * peak, 0.004)
        let last = frames.filter { $0.0 >= end - tail }
        var cut: Int? = nil
        var run: [Int] = []
        for (at, level) in last {
            if level < quiet {
                run.append(at)
            } else {
                if run.count >= 3 { cut = run[run.count / 2] + frame / 2 }
                run = []
            }
        }
        if run.count >= 3 { cut = run[run.count / 2] + frame / 2 }
        let chosen = cut ?? (last.min { $0.1 < $1.1 }.map { $0.0 + frame / 2 } ?? end)
        ranges.append(start..<chosen)
        start = chosen
    }
    ranges.append(start..<samples.count)
    return ranges
}

@main
struct FluidTranscribe {
    static func main() async throws {
        guard CommandLine.arguments.count > 1 else {
            FileHandle.standardError.write("usage: fluid-transcribe <model folder>\n".data(using: .utf8)!)
            exit(2)
        }
        let folder = URL(fileURLWithPath: CommandLine.arguments[1])
        let units: MLComputeUnits
        let backend: String
        switch environment["FLUID_UNITS"] {
        case "gpu": units = .cpuAndGPU; backend = "coreml-gpu"
        case "cpu": units = .cpuOnly; backend = "coreml-cpu"
        default: units = .cpuAndNeuralEngine; backend = "coreml-ane"
        }

        let started = Date()
        let models = try AsrModels.loadLocal(from: folder, version: .ultra, encoderComputeUnits: units)
        let version = AsrModelVersion.ultra
        let config = ASRConfig(
            tdtConfig: TdtConfig(blankId: version.blankId), encoderHiddenSize: version.encoderHiddenSize,
            melChunkContext: environment["FLUID_MEL_CONTEXT"].map { $0 == "1" },
            dualDecodeArbitration: environment["FLUID_DUAL_DECODE"] == "1",
            seamGapRepair: environment["FLUID_SEAM_REPAIR"] != "0",
            seamGapRepairMinGapSeconds: environment["FLUID_SEAM_MIN_GAP"].flatMap(Double.init) ?? 1.5)
        let pauseCut = environment["FLUID_PAUSE_CUT"] == "1"
        let manager = AsrManager(config: config)
        try await manager.loadModels(models)
        let layers = await manager.decoderLayerCount
        // The first call compiles the rest of the pipeline: pay for it now.
        var warm = TdtDecoderState.make(decoderLayers: layers)
        _ = try await manager.transcribe([Float](repeating: 0, count: 16_000), decoderState: &warm)
        say(["event": "ready", "model": "parakeet-ultra", "backend": backend,
             "load_seconds": Date().timeIntervalSince(started)])

        while let header = readExactly(4) {
            let count = Int(header.withUnsafeBytes { $0.loadUnaligned(as: UInt32.self).littleEndian })
            guard let bytes = readExactly(count * 4) else { break }
            let samples = bytes.withUnsafeBytes { Array($0.bindMemory(to: Float.self)) }
            if samples.isEmpty {
                say(["segments": []])
                continue
            }
            let chunkStarted = Date()
            do {
                var texts: [String] = []
                for range in pauseCut ? pauseCuts(samples) : [0..<samples.count] {
                    var state = TdtDecoderState.make(decoderLayers: layers)
                    let result = try await manager.transcribe(Array(samples[range]), decoderState: &state)
                    texts.append(result.text.trimmingCharacters(in: .whitespacesAndNewlines))
                }
                let text = texts.filter { !$0.isEmpty }.joined(separator: " ")
                let end = Double(samples.count) / 16_000
                say(["segments": text.isEmpty ? [] : [["start": 0.0, "end": end, "text": text]],
                     "seconds": Date().timeIntervalSince(chunkStarted)])
            } catch {
                say(["error": "\(error)"])
            }
        }
    }
}
