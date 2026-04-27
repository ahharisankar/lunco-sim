# lunco-obc

On-Board Computer (OBC) emulation for LunCoSim.

## What This Crate Does

This crate implements the hardware-faithful signal processing interface between the digital Flight Software (FSW) and the physical simulation world.

- **DAC (Digital-to-Analog Conversion)** — Maps `i16` register values (from `DigitalPort`) to `f32` physical units (in `PhysicalPort`).
- **ADC (Analog-to-Digital Conversion)** — Samples `f32` sensor data into `i16` registers for software consumption.
- **Quantization & Scaling** — Simulates the resolution limits and hardware scaling (gain) defined by simulation `Wire`s.
- **Bounds Safety** — Hard-clamps values at the 16-bit register limits to emulate realistic hardware saturation.

## Architecture

The OBC layer acts as the **Signal Processing** bridge (Level 2) in the vessel control architecture.

```
lunco-obc/
  ├── DAC Pathway — scale_digital_to_physical system
  └── ADC Pathway — scale_physical_to_digital system
```

### The Wiring Layer

Scaling is determined by the `Wire` component, which defines the relationship between a source and a target port:

```rust
// A wire with scale 100.0 means:
// Digital 32767 -> Physical 100.0 (DAC)
// Physical 50.0  -> Digital 16383 (ADC)
commands.spawn(Wire {
    source: digital_port,
    target: physical_port,
    scale: 100.0,
});
```

## Usage

```rust
app.add_plugins(LunCoObcPlugin);
```

## See Also

- `lunco-core` — Defines the `DigitalPort`, `PhysicalPort`, and `Wire` primitives.
- `lunco-fsw` — The primary consumer of digital ports.
- `lunco-hardware` — The primary consumer of physical ports.
