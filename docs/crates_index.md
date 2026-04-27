# LunCoSim Crates Index

This document provides a comprehensive index of all crates in the LunCoSim workspace, categorized by their functional domain and architectural responsibility. It serves as a navigation guide for both developers and AI agents.

---

## 1. Workspace & Core Foundation
Low-level primitives, document systems, and cross-cutting concerns (storage, assets, theming).

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-core`** | Core primitives (`DigitalPort`, `PhysicalPort`, `CommandMessage`), coordinate systems, and canonical diagram data types. |
| **`lunco-workspace`** | Headless editor session management: open Twins, active documents, perspectives, and recents. |
| **`lunco-twin`** | The simulation unit on disk: folder structure, `twin.toml` manifest parsing, and file indexing. |
| **`lunco-doc`** | Foundation for structured artifacts (Modelica, USD, SysML) with built-in undo/redo logic. |
| **`lunco-storage`** | I/O abstraction layer (Native FS, Memory, and future WASM/Remote backends). |
| **`lunco-assets`** | Unified asset management: cache resolution, versioned downloads, and texture processing. |
| **`lunco-cache`** | Generic resource cache with in-flight deduplication for resolved URIs and parsed artifacts. |
| **`lunco-theme`** | Centralized design tokens (Catppuccin-based) for consistent UI across all panels and domains. |
| **`lunco-command-macro`** | Procedural macros for the typed command system (re-exported by `lunco-core`). |
| **`lunco-doc-bevy`** | Bevy ECS integration for the Document System, providing canonical lifecycle events and journals. |

---

## 2. Simulation Engine
The "Laws of Nature"—celestial mechanics, environmental state, terrain, and co-simulation orchestration.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-celestial`** | High-precision orbital mechanics (Ephemeris), gravity, and Sphere of Influence (SOI) transitions. |
| **`lunco-environment`** | Per-entity position-dependent environment state (atmosphere, radiation, local gravity). |
| **`lunco-terrain`** | Procedural QuadSphere terrain generation, LOD subdivision, and heightmap-based collision. |
| **`lunco-cosim`** | Multi-engine orchestration (Modelica, FMU, GMAT, Avian) via explicit input/output wiring. |

---

## 3. Vessel Control & Hardware
The "Brains and Brawn"—Flight Software (FSW), On-Board Computer (OBC), mobility physics, and robotics assembly.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-mobility`** | Physics models for planetary rovers using high-performance raycast-based wheel/suspension logic. |
| **`lunco-robotics`** | High-level assembly logic and rover structural definitions. |
| **`lunco-avatar`** | Human-interaction layer: composable camera behaviors (SpringArm, Orbit) and control intents. |
| **`lunco-obc`** | Hardware interface emulation (ADC/DAC) between digital FSW registers and physical units. |
| **`lunco-fsw`** | Decentralized Flight Software architecture for coordinating vessel subsystems (GNC, Power, etc.). |
| **`lunco-hardware`** | Concrete implementations of physical actuators and sensors bridging ports to the physics engine. |
| **`lunco-controller`** | Translation of raw user input (Keyboard/Gamepad) into typed commands for FSW. |

---

## 4. USD Integration Layer
Modular bridge between OpenUSD and Bevy, covering visuals, physics, and simulation metadata.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-usd`** | High-level USD orchestrator and mapper for LunCo-specific engineering metadata (`lunco:*`). |
| **`lunco-usd-bevy`** | Core visual bridge: maps USD hierarchy, shapes, and transforms to Bevy entities/components. |
| **`lunco-usd-avian`** | Physics bridge: maps `USDPhysics` schemas (RigidBody, Colliders) to Avian3D components. |
| **`lunco-usd-sim`** | Intercepts specialized simulation schemas (e.g., PhysX Vehicles) and maps them to LunCo models. |
| **`lunco-usd-composer`** | Handles USD asset path resolution and stage flattening for complex multi-file assets. |

---

## 5. Networking & API
External communication, ECS replication, telemetry extraction, and distributed attributes.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-networking`** | Multiplayer layer: transport-agnostic replication, authentication, and collaborative edit logs. |
| **`lunco-api`** | Transport-agnostic API core: introspection-based command discovery and ULID entity registry. |
| **`lunco-telemetry`** | Generic reflection-based data extraction engine for "No-Code" telemetry mirroring. |
| **`lunco-attributes`** | String-based distributed tuning registry for mapping SysML paths to raw ECS memory. |

---

## 6. Workbench & UI Tools
The editor shell, visualization framework, generic 2D canvas, and sandbox editing tools.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-workbench`** | The IDE-like frame: docking engine, perspective presets, and panel registration. |
| **`lunco-ui`** | Reusable UI infrastructure: cached widgets, 3D world panels, and command builders. |
| **`lunco-viz`** | Domain-agnostic visualization: SignalRegistry, LinePlots, and future 3D/Rerun bridges. |
| **`lunco-canvas`** | Stateful 2D scene editor substrate for diagrams and annotation overlays. |
| **`lunco-sandbox-edit`** | In-scene editing tools: spawn systems, transform gizmos, and inspector panels. |

---

## 7. Scripting & Modeling
Logic engines for dynamic simulation behavior and industrial modeling.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-modelica`** | Modelica integration: AST-based editing, compilation via Rumoca, and diagram visualization. |
| **`lunco-scripting`** | Reflected memory bridge for Python and Lua as first-class logic providers. |

---

## 8. Applications
Primary entry points and simulation assembly targets.

| Crate | Responsibility |
| :--- | :--- |
| **`lunco-client`** | The main simulation client assembling all plugins into a cohesive application. |

---

## Detailed Crate Responsibilities

### lunco-core
**Layer: 1 (Foundation)**
The bedrock of the simulation. Defines the `DigitalPort` and `PhysicalPort` architectural primitives that allow software and hardware to talk. It also owns the `CommandMessage` system, which is the primary way to trigger actions in the simulation, and the `ComponentGraph`, which serves as the canonical data structure for all 2D diagram visualizations (Modelica, FSW, SysML).

### lunco-celestial
**Layer: 2 (Domain)**
Handles the large-scale spatial truth. Implements planetary ephemeris (where is Mars right now?), body-fixed rotation, and the Sphere of Influence (SOI) system that automatically transitions entities between coordinate grids (e.g., from Earth orbit to Lunar orbit). It provides gravity vectors for every entity based on their current body.

### lunco-cosim
**Layer: 2 (Domain)**
The "Master Clock" for multi-engine simulations. It uses a wire-and-socket model to connect variables between different simulation solvers. For example, it can wire a Modelica battery model's `voltage` output to an OBC's sensor input, or an Avian physics position to a Modelica solar panel's `height` input. It follows FMI/SSP principles for causality and propagation.

### lunco-networking
**Layer: 2b (Middleware)**
A transparent shim that adds multiplayer capabilities. It uses `renet2` for transport and `bevy_replicon` for ECS state sync. It features a layered authentication model and a collaborative "Edit Log" that records every sandbox action (spawns, moves, deletes) to ensure all clients converge to the same state. It is designed to be removable for single-player performance.

### lunco-usd-bevy
**Layer: 3 (Visual)**
The primary bridge for OpenUSD. It recursively walks a USD stage and spawns Bevy entities for every Prim, mapping standard USD visuals (Cubes, Spheres, Meshes) and `xformOp` transforms. It allows designers to author scenes in standard tools like Omniverse or Blender and have them appear instantly in the simulation.

### lunco-workbench
**Layer: 4 (UI Shell)**
Provides the engineering-IDE frame. It handles the docking engine (tabs, splits, floats), the "Perspective" system (named layout presets like "Build" or "Simulate"), and the Twin Browser. It acts as the host for every other domain's UI panels.

### lunco-modelica
**Layer: 7 (Modeling)**
Integrates the Modelica language. It provides an AST-based editor where every change is a semantic operation (e.g., `AddComponent`) rather than just a text edit. It compiles models via a worker process and allows them to be used as `SimComponent`s in the co-simulation loop.
