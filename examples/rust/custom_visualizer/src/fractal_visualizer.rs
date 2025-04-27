use rerun::external::{
    re_renderer,
    re_viewer_context::{
        self, IdentifiedViewSystem, ViewContext, ViewContextCollection, ViewQuery,
        ViewSystemExecutionError, ViewSystemIdentifier, VisualizerQueryInfo, VisualizerSystem,
    },
};

use crate::{fractal_archetype::Fractal, fractal_renderer::FractalDrawData};

#[derive(Default)]
pub struct FractalVisualizer {}

impl IdentifiedViewSystem for FractalVisualizer {
    fn identifier() -> ViewSystemIdentifier {
        "Fractal".into()
    }
}

impl VisualizerSystem for FractalVisualizer {
    fn visualizer_query_info(&self) -> VisualizerQueryInfo {
        VisualizerQueryInfo::from_archetype::<Fractal>()
    }

    fn execute(
        &mut self,
        ctx: &ViewContext<'_>,
        _query: &ViewQuery<'_>,
        _context_systems: &ViewContextCollection,
    ) -> Result<Vec<re_renderer::QueueableDrawData>, ViewSystemExecutionError> {
        let draw_data = FractalDrawData::new(ctx.render_ctx());

        Ok(vec![draw_data.into()])
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn fallback_provider(&self) -> &dyn re_viewer_context::ComponentFallbackProvider {
        self
    }
}

// Implements a `ComponentFallbackProvider` trait for the `FractalVisualizer`.
// It is left empty here but could be used to provides fallback values for optional components in case they're missing.
use rerun::external::re_types;
re_viewer_context::impl_component_fallback_provider!(FractalVisualizer => []);
