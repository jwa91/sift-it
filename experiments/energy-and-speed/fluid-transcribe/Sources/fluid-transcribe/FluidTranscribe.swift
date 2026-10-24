// `fluid-transcribe <folder with Parakeet Ultra's .mlmodelc files>`: sift-it-transcribe's protocol on FluidAudio.
//
// It loads the models, says so on stdout, then reads chunks of sound from stdin until it closes: a little-endian u32
// sample count, then that many little-endian f32 samples (16 kHz mono). For each chunk it writes one JSON line:
//   {"event":"ready","model":"parakeet-ultra","backend":"coreml-ane","load_seconds":…}
//   {"segments":[{"start":0,"end":…,"text":"…"}],"seconds":…}
// FLUID_UNITS=ane|gpu|cpu picks where the encoder runs (ane by default; the decoder and joint stay on the ANE).
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
        switch ProcessInfo.processInfo.environment["FLUID_UNITS"] {
        case "gpu": units = .cpuAndGPU; backend = "coreml-gpu"
        case "cpu": units = .cpuOnly; backend = "coreml-cpu"
        default: units = .cpuAndNeuralEngine; backend = "coreml-ane"
        }

        let started = Date()
        let models = try AsrModels.loadLocal(from: folder, version: .ultra, encoderComputeUnits: units)
        let version = AsrModelVersion.ultra
        let config = ASRConfig(tdtConfig: TdtConfig(blankId: version.blankId), encoderHiddenSize: version.encoderHiddenSize)
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
                var state = TdtDecoderState.make(decoderLayers: layers)
                let result = try await manager.transcribe(samples, decoderState: &state)
                let text = result.text.trimmingCharacters(in: .whitespacesAndNewlines)
                let end = Double(samples.count) / 16_000
                say(["segments": text.isEmpty ? [] : [["start": 0.0, "end": end, "text": text]],
                     "seconds": Date().timeIntervalSince(chunkStarted)])
            } catch {
                say(["error": "\(error)"])
            }
        }
    }
}
