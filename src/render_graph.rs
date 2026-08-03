//! Per-frame engawa render-graph routing — M3-C1.
//!
//! ONE typed [`engawa::RenderGraph`] carries every enabled post effect
//! per frame: mado's own scene pipelines (rects / kitty images /
//! glyphon text) render into the catalog [`SCENE`] texture, the
//! enabled [`CatalogEffect`]s chain `SCENE → … → OUT` in ascending
//! catalog-priority order (post range 200..=799), and [`OUT`] is bound
//! to the swapchain surface at dispatch. Disabled effects contribute
//! ZERO nodes — an empty set means no graph exists at all and the
//! scene renders straight to the surface.
//!
//! The [`engawa::CompiledGraph`] is CACHED keyed by
//! (enabled-effect-set, resolution): recompiled only on effect toggle
//! or resize, never per frame. [`FrameGraphCache::compile_count`] is
//! the mechanical proof surface the steady-state test pins.
//!
//! This module is deliberately GPU-free (pure graph/bindings data) so
//! the whole effect-set power set is provable in plain unit tests; the
//! renderer side ([`crate::render`]) owns leases, uniform buffers, and
//! the `WgpuDispatcher` call.

use engawa::{
    CompiledGraph, RenderGraph, ResourceBindings, ResourceHandle, ResourceId, ResourceKind,
};
use engawa_wgpu::catalog::{CATALOG_SAMPLER, CatalogEffect, OUT, SCENE};

/// Enabled-effect set — one bit per [`CatalogEffect::ALL`] entry (the
/// mechanical registry position IS the bit index, so the set can never
/// drift from the catalog). Fits `u16` while the catalog has ≤ 16
/// effects; the forcing test pins that bound. (Widened from `u8` when
/// `window_depth` made the catalog 9 effects.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct EffectSet(u16);

impl EffectSet {
    pub const EMPTY: Self = Self(0);

    fn bit(effect: CatalogEffect) -> u16 {
        // CatalogEffect::ALL is emitted by pleme-allvariants-derive —
        // every variant is present by construction, so the position
        // lookup cannot miss (absence is unrepresentable upstream).
        let idx = CatalogEffect::ALL
            .iter()
            .position(|e| *e == effect)
            .expect("CatalogEffect::ALL is derive-total");
        1u16 << idx
    }

    // Builder used by the test matrices; production sites insert().
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn with(mut self, effect: CatalogEffect) -> Self {
        self.0 |= Self::bit(effect);
        self
    }

    pub fn insert(&mut self, effect: CatalogEffect) {
        self.0 |= Self::bit(effect);
    }

    #[must_use]
    pub fn contains(self, effect: CatalogEffect) -> bool {
        self.0 & Self::bit(effect) != 0
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Enabled effects in render order — ascending catalog priority
    /// (bloom 300 → glow 400 → aurora 450 → snow 500 → scanlines 600 →
    /// crt 650 → colorblind 700 → grain 750 last). The chain wiring
    /// below consumes this order.
    pub fn iter_render_order(self) -> impl Iterator<Item = CatalogEffect> {
        let mut effects: Vec<CatalogEffect> = CatalogEffect::ALL
            .iter()
            .copied()
            .filter(move |e| self.contains(*e))
            .collect();
        effects.sort_by_key(|e| e.priority());
        effects.into_iter()
    }
}

/// Intermediate chain-target resource ids: `CHAIN_IDS[i]` receives the
/// i-th enabled effect's output when a later effect follows it (the
/// last effect always writes [`OUT`]). A chain of N effects needs
/// N-1 intermediates, so the table holds `ALL.len() - 1` ids — the
/// forcing test pins the length, so a new catalog effect cannot land
/// without growing this table in the same change.
pub const CHAIN_IDS: [&str; 8] = [
    "mado:chain:0",
    "mado:chain:1",
    "mado:chain:2",
    "mado:chain:3",
    "mado:chain:4",
    "mado:chain:5",
    "mado:chain:6",
    "mado:chain:7",
];

/// Cache key — the only two inputs that change the compiled topology
/// or the offscreen texture dimensions the bindings will lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphKey {
    pub effects: EffectSet,
    pub width: u32,
    pub height: u32,
}

/// A compiled frame graph plus everything the renderer needs to
/// dispatch it: the engawa-level resource handles and the texture
/// resource ids (chain intermediates + per-effect aux) that must be
/// pool-leased and bound per frame in addition to SCENE/OUT.
pub struct CompiledFrameGraph {
    pub key: GraphKey,
    pub graph: CompiledGraph,
    pub bindings: ResourceBindings,
    pub intermediates: Vec<ResourceId>,
}

/// Single-slot compile cache. One key is live at a time (mado renders
/// one window); a toggle or resize replaces it and bumps the counter.
#[derive(Default)]
pub struct FrameGraphCache {
    cached: Option<CompiledFrameGraph>,
    compile_count: u64,
}

impl FrameGraphCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total compiles since construction — the steady-state proof
    /// surface: N frames at a stable key must leave this at 1.
    /// Consumed by the unit matrix here and the `gpu_tests` live-route
    /// golden; no production read (it is a proof counter, not a knob).
    #[cfg_attr(not(any(test, feature = "gpu_tests")), allow(dead_code))]
    #[must_use]
    pub fn compile_count(&self) -> u64 {
        self.compile_count
    }

    /// Resolve the compiled graph for `key`, recompiling only when the
    /// key changed. `None` ⇔ the effect set is empty (zero nodes — the
    /// caller renders the scene straight to the surface). An empty-set
    /// frame leaves the previous compile cached, so toggling an effect
    /// off and back on at the same resolution costs zero recompiles.
    pub fn ensure(&mut self, key: GraphKey) -> Option<&CompiledFrameGraph> {
        if key.effects.is_empty() {
            return None;
        }
        let stale = self.cached.as_ref().is_none_or(|c| c.key != key);
        if stale {
            self.cached = Some(compile_frame_graph(key));
            self.compile_count += 1;
        }
        self.cached.as_ref()
    }
}

/// Build + compile the canonical chain graph for one key. The shape
/// mirrors `CatalogEffect::graph()` (the catalog's single-effect
/// proof) extended to an N-effect chain: SCENE → e₁ → chain:0 → e₂ →
/// … → OUT, every params uniform + the shared sampler declared as
/// graph inputs, bloom's aux ping-pong textures declared as node
/// resources.
fn compile_frame_graph(key: GraphKey) -> CompiledFrameGraph {
    let enabled: Vec<CatalogEffect> = key.effects.iter_render_order().collect();

    let mut g = RenderGraph::default()
        .with_resource(
            SCENE,
            ResourceKind::Texture {
                width: None,
                height: None,
                format: None,
                sample_count: None,
            },
        )
        .with_resource(
            OUT,
            ResourceKind::Texture {
                width: None,
                height: None,
                format: None,
                sample_count: None,
            },
        )
        .with_resource(CATALOG_SAMPLER, ResourceKind::Sampler)
        .with_input(SCENE)
        .with_input(CATALOG_SAMPLER)
        .with_output(OUT);
    let mut bindings = ResourceBindings::new()
        .with(SCENE, ResourceHandle::Texture(SCENE.into()))
        .with(OUT, ResourceHandle::Texture(OUT.into()));
    let mut intermediates: Vec<ResourceId> = Vec::new();

    for effect in &enabled {
        let params_size =
            u32::try_from(effect.params_size()).expect("catalog params structs are tiny");
        g = g
            .with_resource(
                effect.params_resource(),
                ResourceKind::Uniform {
                    size_bytes: params_size,
                },
            )
            .with_input(effect.params_resource());
        for (id, kind) in effect.aux_resources() {
            g = g.with_resource(id, kind);
            bindings.insert(id, ResourceHandle::Texture(id.into()));
            intermediates.push(id.into());
        }
    }

    let mut input: ResourceId = SCENE.into();
    let last = enabled.len().saturating_sub(1);
    for (i, effect) in enabled.iter().enumerate() {
        let output: ResourceId = if i == last {
            OUT.into()
        } else {
            let id = CHAIN_IDS[i];
            g = g.with_resource(
                id,
                ResourceKind::Texture {
                    width: None,
                    height: None,
                    format: None,
                    sample_count: None,
                },
            );
            bindings.insert(id, ResourceHandle::Texture(id.into()));
            intermediates.push(id.into());
            id.into()
        };
        for node in effect.lower(&input, &output) {
            g = g.with_node(node);
        }
        input = output;
    }

    // Compile failure is unreachable for every constructible key: the
    // power-set forcing test below compiles ALL 2^N effect sets, so a
    // topology bug cannot land without that test failing first.
    let graph = g
        .compile()
        .expect("frame graph compiles for every effect set");

    CompiledFrameGraph {
        key,
        graph,
        bindings,
        intermediates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_power_set() -> impl Iterator<Item = EffectSet> {
        // Range in u32 so the subset count (2^ALL.len()) can't overflow
        // the u16 bitset's `1 << len`; each subset value fits the u16
        // bitset (bits 0..=15), so the cast back is lossless.
        let bound: u32 = 1u32 << CatalogEffect::ALL.len();
        (0u32..bound).map(|v| EffectSet(v as u16))
    }

    fn key(effects: EffectSet) -> GraphKey {
        GraphKey {
            effects,
            width: 640,
            height: 480,
        }
    }

    /// FORCING FUNCTION — the bit registry can hold every catalog
    /// effect and each gets a distinct bit. Aggregated matrix-style.
    #[test]
    fn effect_set_bits_are_total_and_distinct() {
        assert!(
            CatalogEffect::ALL.len() <= 16,
            "EffectSet is a u16 bitset — widen it before the catalog grows past 16 effects"
        );
        let mut failures: Vec<String> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for effect in CatalogEffect::ALL.iter().copied() {
            let set = EffectSet::EMPTY.with(effect);
            if !set.contains(effect) {
                failures.push(std::format!(
                    "{effect:?}: insert→contains round-trip failed"
                ));
            }
            if !seen.insert(set.0) {
                failures.push(std::format!("{effect:?}: bit collides with another effect"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} effect-set failures:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// FORCING FUNCTION — one chain id per possible intermediate. A
    /// new catalog effect grows ALL and fails this until `CHAIN_IDS`
    /// grows in the same change.
    #[test]
    fn chain_id_table_matches_catalog_size() {
        assert_eq!(
            CHAIN_IDS.len(),
            CatalogEffect::ALL.len() - 1,
            "a chain of N effects needs N-1 intermediates"
        );
    }

    /// Every one of the 2^N effect sets compiles, and its node count
    /// is exactly the sum of the enabled effects' lowerings (bloom 4,
    /// everything else 1) — disabled effects contribute ZERO nodes.
    /// This is the proof the production `expect` in
    /// `compile_frame_graph` leans on.
    #[test]
    fn every_effect_subset_compiles_with_exact_node_count() {
        let mut failures: Vec<String> = Vec::new();
        for set in full_power_set() {
            if set.is_empty() {
                continue;
            }
            let expected_nodes: usize = set
                .iter_render_order()
                .map(|e| {
                    let scene: ResourceId = SCENE.into();
                    let out: ResourceId = OUT.into();
                    e.lower(&scene, &out).len()
                })
                .sum();
            let compiled = compile_frame_graph(key(set));
            if compiled.graph.node_count() != expected_nodes {
                failures.push(std::format!(
                    "{set:?}: {} nodes, expected {expected_nodes}",
                    compiled.graph.node_count()
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} subsets mis-compiled:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// Every node input/output in every subset's compiled graph has a
    /// `ResourceHandle` in the bindings — the dispatch-time
    /// `MissingBinding` error is unreachable by construction.
    #[test]
    fn every_subset_binds_every_node_edge() {
        let mut failures: Vec<String> = Vec::new();
        for set in full_power_set() {
            if set.is_empty() {
                continue;
            }
            let compiled = compile_frame_graph(key(set));
            for node in compiled.graph.iter_nodes() {
                for id in node.inputs.iter().chain(node.outputs.iter()) {
                    // Uniform/sampler resources are bound per frame via
                    // BoundResources; the engawa-level handle map only
                    // needs the texture edges the dispatcher validates.
                    if compiled.bindings.get(id).is_none()
                        && id.as_str() != CATALOG_SAMPLER
                        && !id.as_str().ends_with(":params")
                    {
                        failures.push(std::format!("{set:?}: {id:?} unbound"));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} unbound edges:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    /// MECHANICAL steady-state proof: N frames at one key = 1 compile;
    /// an effect toggle = +1; a resize = +1; an all-off frame = +0.
    #[test]
    fn compile_count_moves_only_on_toggle_or_resize() {
        let mut cache = FrameGraphCache::new();
        let base = key(EffectSet::EMPTY.with(CatalogEffect::Colorblind));

        for _ in 0..32 {
            assert!(cache.ensure(base).is_some());
        }
        assert_eq!(
            cache.compile_count(),
            1,
            "steady state must compile exactly once"
        );

        let toggled = GraphKey {
            effects: base.effects.with(CatalogEffect::Crt),
            ..base
        };
        cache.ensure(toggled);
        assert_eq!(
            cache.compile_count(),
            2,
            "effect toggle must recompile once"
        );

        let resized = GraphKey {
            width: 800,
            height: 600,
            ..toggled
        };
        for _ in 0..8 {
            cache.ensure(resized);
        }
        assert_eq!(cache.compile_count(), 3, "resize must recompile once");

        assert!(cache.ensure(key(EffectSet::EMPTY)).is_none());
        assert_eq!(
            cache.compile_count(),
            3,
            "empty set must not compile a graph"
        );
    }

    /// Chain wiring is priority-ordered: the rendered order for the
    /// full set must be ascending `priority()` regardless of the
    /// declaration order bits were set in.
    #[test]
    fn render_order_is_ascending_priority() {
        let mut set = EffectSet::EMPTY;
        for effect in CatalogEffect::ALL.iter().copied() {
            set.insert(effect);
        }
        let priorities: Vec<u16> = set
            .iter_render_order()
            .map(CatalogEffect::priority)
            .collect();
        let mut sorted = priorities.clone();
        sorted.sort_unstable();
        assert_eq!(
            priorities, sorted,
            "chain must follow catalog priority order"
        );
    }
}
