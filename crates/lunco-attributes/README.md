# lunco-attributes

Distributed Attribute Management and Tuning Registry for LunCoSim.

## What This Crate Does

This crate implements the simulation's **"Tuning Registry"**—a bridge between raw ECS memory and external System Modeling (SysML) definitions.

- **String-Based Addressing** — Reference simulation parameters via human-readable paths (e.g., `"vessel.rover1.suspension.k"`).
- **Digital Twin Alignment** — Paths map 1:1 with SysML v2 architectural models.
- **Headless Tuning** — Enables external processes (Python scripts, optimizers) to mutate live simulation state via reflection.
- **Type-Safe Reflection** — Uses Bevy's `Reflect` system to resolve field paths and apply values with appropriate type conversion.

## Architecture

`LunCoAttributesPlugin` provides the foundational registry and command observers.

```
lunco-attributes/
  ├── AttributeRegistry  — HashMap mapping SysML paths to ECS locations
  ├── AttributeAddress   — Triple of (Entity, Component, FieldPath)
  ├── SetAttribute       — Event to request a mutation
  └── ApplyReflectedSet  — Command that performs reflection-based mutation
```

### The "Why": Path Mirroring

Hardcoding every tunable value creates brittle code. By providing a generic reflection layer, we allow the simulation to mirror the design documentation exactly. Any field tagged as `Reflect` can be exposed via a path in the `AttributeRegistry`.

## Usage

```rust
// Register a path
registry.map.insert(
    "vessel.rover1.motor_left.max_torque".to_string(), 
    AttributeAddress {
        entity: motor_entity,
        component: "PhysicalPort".to_string(),
        field: "value".to_string(),
    }
);

// Trigger a mutation
commands.trigger(SetAttribute {
    path: "vessel.rover1.motor_left.max_torque".to_string(),
    value: TelemetryValue::F64(95.5),
});
```

## See Also

- `lunco-core` — Defines `TelemetryValue` and basic simulation primitives.
- `lunco-telemetry` — The counterpart for extracting data out of the simulation.
