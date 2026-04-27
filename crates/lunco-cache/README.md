# lunco-cache

Generic resource cache for LunCoSim with in-flight deduplication and shared parsed artifacts.

## What This Crate Does

This crate provides a domain-agnostic substrate for loading and caching expensive resources (Modelica ASTs, USD Stages, SysML elements).

- **In-flight Deduplication** — N concurrent requests for the same resource collapse onto a single background task.
- **Shared Artifacts** — Uses `Arc<V>` to share parsed data across multiple consumers without cloning.
- **Non-blocking Driving** — Polled from Bevy's `Update` system; uses Bevy's task pools for I/O and CPU-heavy transforms.
- **Domain-Pluggable** — Implement the `ResourceLoader` trait to add caching to any domain.

## Architecture

The system decouples the **Loading** logic (domain-specific) from the **Caching** logic (generic).

```
lunco-cache/
  ├── ResourceLoader  — Trait for defining how to resolve and parse a resource
  ├── ResourceCache   — The central registry managing Ready and Pending states
  └── ResourceState   — Enum representing Ready(Arc<V>) or Failed(Arc<str>)
```

### Why Bevy-flavored Tasks?

Using `bevy::tasks::Task<T>` keeps the "spawn side" (AsyncComputeTaskPool) and "poll side" (`ResourceCache::drive`) decoupled without requiring a separate executor dependency.

## Usage

Wrap the generic `ResourceCache` in a domain-specific newtype:

```rust
struct MyLoader;
impl ResourceLoader for MyLoader { ... }

#[derive(Resource)]
pub struct MyCache(ResourceCache<MyLoader>);
```

In a Bevy system:

```rust
fn drive_cache(mut cache: ResMut<MyCache>) {
    cache.0.drive();
}
```

## See Also

- `lunco-modelica` — Uses this for the `ClassCache`.
- `lunco-usd` — Planned consumer for USD asset caching.
