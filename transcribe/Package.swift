// swift-tools-version: 6.2
// sift-it-transcribe, the speech helper: Parakeet Ultra on the Neural Engine, through FluidAudio.
import PackageDescription

let package = Package(
    name: "sift-it-transcribe",
    platforms: [.macOS(.v14)],
    dependencies: [
        // Its speech to text alone: no traits, so none of its text to speech, diarization or text normalization.
        .package(
            url: "https://github.com/FluidInference/FluidAudio.git",
            revision: "c79c4106b954f81205113c3343e5a7e59a1089b5",
            traits: []
        )
    ],
    targets: [
        .executableTarget(
            name: "sift-it-transcribe",
            dependencies: [.product(name: "FluidAudio", package: "FluidAudio")]
        )
    ]
)
