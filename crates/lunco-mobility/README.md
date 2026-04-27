# lunco-mobility

Surface mobility and traction physics for LunCoSim planetary rovers.

## What This Crate Does

This crate implements high-performance physics models for surface vehicles, focusing on stability and realistic ground interaction.

- **Raycast-Based Wheel Model** — Uses emulated suspension rays instead of complex mesh-to-mesh collision for high performance on irregular terrain.
- **Suspension Physics** — Spring-damper system (Hooke's Law) for realistic vehicle dynamics and oscillation suppression.
- **Traction & Friction** — Coulomb friction model for longitudinal drive and lateral skid/slip behaviors.
- **Steering Mixing** — Support for Differential (Skid) drive and Ackermann steering architectures.
- **Joint-Based Suspension** — Prismatic joint support for vehicles with physical collision wheels.

## Architecture

Mobility logic runs in the `FixedUpdate` schedule, chain-linking suspension and traction systems.

```
lunco-mobility/
  ├── WheelRaycast       — The core high-performance wheel component
  ├── Suspension         — Spring-damper configuration for joints
  ├── DifferentialDrive  — Control mixing for skid-steer rovers
  ├── AckermannSteer     — Control mixing for articulated steering
  └── systems.rs         — Ray-world intersection and force application logic
```

### The Raycast Advantage

By using a single ray per wheel:
1. We eliminate wheel "snagging" on terrain geometry.
2. We ensure numeric stability during high-speed travel.
3. We provide a clean interface for visual wheel mesh positioning.

## Usage

```rust
app.add_plugins(LunCoMobilityPlugin);

// Spawning a raycast wheel
commands.spawn((
    WheelRaycast {
        rest_length: 0.5,
        spring_k: 10000.0,
        ..default()
    },
    RayCaster::default(), // From avian3d
));
```

## See Also

- `lunco-controller` — Translates user input into the `DriveRover` events consumed here.
- `lunco-hardware` — Provides the physical actuators (motors, brakes) that mobility systems interface with.
- `avian3d` — The underlying physics engine for force integration.
