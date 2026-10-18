# Audio fixtures

For `adapters::macos::av::tests`, which check that sound decodes the same every time (docs/audio-decoding.md).

- `priming.mp3`: the Spanish voice memo `scripts/make-testbed.sh` makes. Its track holds 170496 samples at 22050 Hz,
  and its decoder sometimes puts out 529 more past the end.
- `partial.m4a`: the first 1.235 seconds of the testbed's podcast, as mono AAC at 22050 Hz, ending in a partial buffer
  (27232 samples):

  ```sh
  ffmpeg -i testbed/Recordings/episode-12.m4a -t 1.235 -ar 22050 -ac 1 -c:a aac -b:a 32k tests/audio/partial.m4a
  ```
