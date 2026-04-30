# LunCoSim USD System

## Overview

The USD subsystem loads rover definitions from USD (Universal Scene Description) files and
maps them to Bevy entities with Avian3D physics and LunCoSim simulation components. All rover
definitions are declarative `.usda` files — no procedural code needed.

## Architecture

### Pipeline

```
┌─────────────┐    ┌──────────────────┐    ┌──────────────────┐
│  .usda file │───>│  UsdBevyPlugin   │───>│  UsdAvianPlugin  │
│  (rover)    │    │  (visual sync)   │    │  (physics map)   │
└─────────────┘    └──────────────────┘    └────────┬─────────┘
                                                    │
                     ┌──────────────────┐           │
                     │  UsdSimPlugin    │<──────────┘
                     │  (FSW, wheels,   │
                     │   steering)      │
                     └──────────────────┘
```

### Three Plugin Phases

1. **UsdBevyPlugin** — Spawns child entities for USD prims and attaches meshes + transforms.
2. **UsdAvianPlugin** — Maps USD physics attributes (`physics:rigidBodyEnabled`,
   `physics:mass`, `physics:collisionEnabled`) to Avian3D components.
3. **UsdSimPlugin** — Detects simulation schemas (`PhysxVehicleContextAPI`,
   `PhysxVehicleWheelAPI`, `PhysxVehicleDriveSkidAPI`) and creates `WheelRaycast`,
   `FlightSoftware`, `DifferentialDrive`, etc.

All three plugins use **deferred processing systems** that run in the `Update` schedule
**after** `sync_usd_visuals`, ensuring the USD asset is fully loaded before any component
mapping occurs. This avoids the async loading race condition where the observer fires before
the asset is available.

## Rover Definitions

### Consolidated Base Files

Only **2 base rover files** exist. All scene instances reference these files with overrides:

| File | Steering | Default Wheel Type |
|------|----------|-------------------|
| `skid_rover.usda` | `PhysxVehicleDriveSkidAPI` | `raycast` |
| `ackermann_rover.usda` | `PhysxVehicleDrive4WAPI` | `raycast` |

### Wheel Type Declaration

The `lunco:wheelType` attribute on the **chassis prim** determines wheel behavior:

```usda
def Cube "MyRover" (
    prepend apiSchemas = ["PhysxVehicleContextAPI", "PhysxVehicleDriveSkidAPI"]
)
{
    # Wheel type: "raycast" (default) or "physical"
    string lunco:wheelType = "raycast"
    ...
}
```

| Wheel Type | Components | Use Case |
|------------|-----------|----------|
| `raycast` (default) | `WheelRaycast`, `RayCaster`, entity splitting | Suspension simulation |
| `physical` | `RigidBody`, `Collider`, `MotorActuator` | Physical collision wheels |

### Entity Layout (Raycast Rover)

```
Rover (root entity)
├── Transform: position set by reference, rotation = identity
├── Components: Vessel, RoverVessel, RigidBody, Collider, Mass,
│               LinearDamping, AngularDamping, Mesh3d,
│               DifferentialDrive (or AckermannSteer), FlightSoftware
│
├── Wheel_FL (child entity — physics)
│   ├── Transform: relative position, rotation = IDENTITY (for raycasting)
│   ├── Components: WheelRaycast, RayCaster (Dir3::NEG_Y), RayHits
│   └── Wheel_FL_visual (grandchild entity — rendering)
│       ├── Transform: (0,0,0), rotation = 90° Z (wheel orientation)
│       └── Components: Mesh3d, MeshMaterial3d, CellCoord
│
└── ... (3 more wheels, same structure)
```

### Wheel Entity Splitting

USD defines each wheel as a **single entity** with both a mesh and a rotation. Raycast wheels
need identity rotation so `RayCaster::new(Dir3::NEG_Y)` casts straight down. The
`process_usd_sim_prims` system splits the USD wheel into:

1. **Physics entity**: identity rotation (correct raycasting), NO mesh
2. **Visual child entity**: 90° Z rotation + mesh (correct rendering)

Physical wheels keep the USD entity as-is (no splitting needed).

## Scene Composition

### External References with Overrides

Scenes reference rover definitions and override parameters locally:

```usda
# Rover instance with color, position, and wheel type overrides
def Cube "Rover1" (
    prepend references = @/vessels/rovers/skid_rover.usda@</SkidRover>
)
{
    color3f primvars:displayColor = (0.8, 0.2, 0.2)  # Override color
    double3 xformOp:translate = (15.0, 5.0, -10.0)   # Override position
    string lunco:wheelType = "physical"               # Override wheel type
}
```

The composition uses a **weak-merge strategy** (`or_insert`): local values always win over
referenced values. This allows unlimited parameter overrides without modifying base files.

### Avatar Definition

Cameras are defined directly in the scene file:

```usda
def Xform "Avatar"
{
    string lunco:avatar = "true"
    string lunco:cameraMode = "freeflight"  # freeflight | orbit | springarm
    float lunco:cameraYaw = 2.51327412287
    float lunco:cameraPitch = -0.3
    double3 xformOp:translate = (-30.0, 15.0, -20.0)
}
```

## File Structure

```
assets/
├── scenes/sandbox/sandbox_scene.usda   # Scene: ground + ramps + rovers + avatar
├── vessels/rovers/
│   ├── skid_rover.usda                 # Base skid-steer rover
│   ├── ackermann_rover.usda            # Base Ackermann-steer rover
│   └── rucheyok/                       # Specialized rover variants
└── components/mobility/
    └── wheel.usda                       # Reusable wheel component

crates/
├── lunco-usd/                           # Re-export crate
├── lunco-usd-bevy/                      # Visual sync: meshes, transforms, children
├── lunco-usd-avian/                     # Physics mapping: RigidBody, Collider, Mass
├── lunco-usd-sim/                       # Simulation: wheels, FSW, steering, avatar
├── lunco-usd-composer/                  # USD composition: reference resolution
└── lunco-sandbox-edit/                  # In-scene editing tools (spawn, gizmo, etc.)
```

## Sandbox Editing Tools

The `lunco-sandbox-edit` crate provides in-scene editing capabilities:

### Spawn Palette
EGUI window with categorized spawnable objects (Rovers, Props, Terrain):
- **Click** an item → ghost follows cursor → **click** in scene to place
- **Drag** an item from palette → **click** in scene to place
- Press **Escape** to cancel

### Transform Gizmo
`transform-gizmo-bevy` integration for manipulating spawned objects:
- **G** key → Translate mode (3-axis arrows)
- **R** key → Rotate mode (3-axis rings)
- Select objects by clicking them

### Inspector Panel
EGUI window showing selected entity's name, transform, and physics parameters.

### Undo
**Ctrl+Z** to revert spawns and transform changes.

### Command-Based Spawning
All spawn operations go through `CommandMessage` (`SPAWN_ENTITY:<entry_id>`), enabling
future CLI spawning:
```rust
commands.trigger(CommandMessage {
    id: 0,
    target: grid_entity,
    name: "SPAWN_ENTITY:ball_dynamic".to_string(),
    args: smallvec![x, y, z, 0.0],
    source: Entity::PLACEHOLDER,
});
```

## Reference Resolution

USD references (e.g., `@/components/mobility/wheel.usda@`) are resolved relative to the
**USD asset root** (`assets/`). The `UsdComposer::flatten()` function walks the directory
tree to find the `assets/` directory and resolves `/`-prefixed absolute paths against it.

## glTF Payloads

Pixar's USD distribution loads `.gltf` / `.glb` through the `UsdGltf`
SdfFileFormat plugin, so a payload like `prepend payload = @./body.glb@` parses
glTF as if it were USD. Our minimal `openusd-rs` has no plugin system, so the
composer recognises non-USD extensions (`glb`, `gltf`, `obj`, `stl`) on
`payload`/`references` and:

1. Skips the USD-text read.
2. Resolves the asset path string per the same rules as USD references — URI
   schemes (`lunco-lib://...`) pass through; `/`-prefixed paths anchor at the
   asset root; plain relatives go against the layer's directory.
3. Synthesises an attribute `lunco:resolvedAsset` on the referencing prim with
   the resolved URI.

`sync_usd_visuals` then reads `lunco:resolvedAsset` and dispatches:

| Mode (`lunco:assetMode`) | Result |
|---|---|
| `"mesh"` | `Handle<Mesh>` from `<uri>#Mesh0/Primitive0` (or `lunco:assetLabel`), attached as `Mesh3d`. Single-mesh path stays compatible with `lunco-usd-avian` collider construction. |
| `"scene"` (default) | `Handle<Scene>` from `<uri>#Scene0`, attached as a child `SceneRoot`. Preserves multi-mesh hierarchy, materials, and lights. |

Example (sandbox uses this for the NASA Mars 2020 Perseverance rover):

```usda
def Xform "Perseverance" (
    prepend payload = @lunco-lib://models/perseverance.glb@
)
{
    string lunco:assetMode = "scene"
    double3 xformOp:translate = (5.0, 0.5, 25.0)
}
```

### Asset URI schemes

| Scheme | Purpose | Resolves to |
|---|---|---|
| (no scheme, relative or `/abs`) | In-tree authored content (default Bevy `assets://`) | `assets/...` |
| `lunco-lib://` | **Workspace-shipped library** — analog to Unreal's `/Engine/`, Blender's "Essentials". Declared in per-crate `Assets.toml`, fetched into the shared cache by `cargo run -p lunco-assets -- download`. Registered as an `AssetSource` in `lunco-client/src/main.rs`. | `<cache>/...` |
| `lunco://` | **Reserved**. Earmarked for the future LunCoSim asset/scene service (multi-user, collaborative, network-backed — analogous to Omniverse's Nucleus). Not registered today; do not use. | — |

The split between `lunco-lib://` (local cache, today) and `lunco://` (future
network protocol) is intentional. Mirrors the way Omniverse keeps shipped
content namespaces distinct from the Nucleus protocol's URI grammar — it lets
the future protocol design `lunco://` from a blank slate without legacy
carve-outs from today's caching needs.

## Coordinate Systems

| System | Up Axis | Forward Axis | Notes |
|--------|---------|--------------|-------|
| USD    | Y       | +Z           | Standard USD convention |
| Bevy   | Y       | -Z           | Right-handed, Z-backward |
| Avian3D| Y       | -Z           | Matches Bevy |

## Adding a New Rover Variant

1. **Create base file** (if new steering type needed):
   ```usda
   def Cube "MyRover" (
       prepend apiSchemas = ["PhysxVehicleContextAPI", "PhysxVehicleDriveSkidAPI"]
   ) {
       string lunco:wheelType = "raycast"
       # ... chassis and wheel definitions ...
   }
   ```

2. **Reference it in the scene** with overrides:
   ```usda
   def Cube "MyInstance" (
       prepend references = @/vessels/rovers/my_rover.usda@</MyRover>
   ) {
       color3f primvars:displayColor = (1, 0, 0)
       double3 xformOp:translate = (10.0, 5.0, 0.0)
   }
   ```

## Testing

All tests load **real USD files** through the same pipeline as runtime:

```bash
cargo test --package lunco-usd
cargo test --package lunco-sandbox-edit
```

Key test files:
- `integration_asset_loading.rs` — verifies full pipeline (composition → Bevy → Avian → Sim)
- `rover_structure.rs` — verifies wheel entity structure (identity rotation + visual child)
- `dump_usd_rover.rs` — dumps complete entity/component state for debugging
