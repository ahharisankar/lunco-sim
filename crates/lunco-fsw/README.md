# lunco-fsw

Flight Software (FSW) and Command Fabric for LunCoSim.

## What This Crate Does

This crate implements the simulation's **"Cerebellum"**—the decentralized control architecture responsible for coordinating vessel subsystems.

- **Decentralized Subsystems** — Subsystems (GNC, Power, Mobility) are independent ECS entities rather than a monolithic script.
- **Asynchronous Command Fabric** — Communication via `CommandMessage` events broadcast over the ECS event bus.
- **Hardware Abstraction** — Decouples semantic logic from physical hardware via the `port_map`.
- **Digital Twin Mirroring** — Maps SysML mnemonics to ECS entities, allowing the same software logic to run against different vehicle manifests.

## Architecture

LunCoSim follows a **Decentralized Subsystem** pattern, mirroring real aerospace hardware.

```
lunco-fsw/
  ├── FlightSoftware    — The primary container for a vessel's software state
  ├── VesselSubsystem   — Marker for autonomous functional units
  ├── port_map          — HashMap<String, Entity> for semantic hardware addressing
  └── command_fabric    — Asynchronous message dispatch logic
```

### The port_map Pattern

Instead of hardcoding Entity IDs, software refers to ports by their SysML mnemonic:

```rust
// Mnemonic: "thruster_main" -> Entity: 42
if let Some(&entity) = fsw.port_map.get("thruster_main") {
    // Actuate the hardware at that entity
}
```

This allows the same Flight Software configuration to work regardless of which specific entities represent the thrusters in a particular simulation run.

## Usage

```rust
app.add_plugins(LunCoFswPlugin);

// Define a vessel's software manifest
commands.spawn((
    FlightSoftware {
        port_map: [("drive_left".into(), port_entity)].into(),
        ..default()
    },
    VesselSubsystem,
));
```

## See Also

- `lunco-obc` — Implements the signal processing (DAC/ADC) between FSW and hardware.
- `lunco-mobility` — Provides high-level mobility observers that interface with FSW.
