// swift-tools-version: 6.0
// Experiment: Parakeet Ultra on the Neural Engine through FluidAudio, speaking sift-it-transcribe's protocol, so one
// driver can time both on the same chunks of sound.
import PackageDescription

let package = Package(
    name: "fluid-transcribe",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/FluidInference/FluidAudio.git", revision: "c79c4106b9")
    ],
    targets: [
        .executableTarget(
            name: "fluid-transcribe",
            dependencies: [.product(name: "FluidAudio", package: "FluidAudio")],
            swiftSettings: [.unsafeFlags(["-Ounchecked"])]
        )
    ]
)
