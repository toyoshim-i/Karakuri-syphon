# karakuri-syphon

A standalone Syphon output sink plugin for the [Karakuri](https://github.com/toyoshim/Karakuri) visual performance system.

## Architecture

`karakuri-syphon` runs as an independent out-of-process helper, communicating with Karakuri over standard I/O pipes using newline-delimited JSON (ndjson).

- **Zero-Copy Video Exchange**: Frames rendered by Karakuri on macOS are backed by `IOSurface`. Karakuri transmits lightweight `surface_id` (32-bit `IOSurfaceID`) tokens across the process boundary. `karakuri-syphon` resolves the `IOSurfaceID` and publishes it via Syphon framework to local VJ/compositing software (Resolume, MadMapper, TouchDesigner, OBS Studio, etc.) with 0 GPU-CPU-GPU memory copies.
- **Fault Isolation**: A crash or stall in the Syphon server never affects Karakuri's real-time engine or window presentation.
- **Protocol**: Implements Karakuri's `output_plugin` wire protocol version 1.

## License

MIT

