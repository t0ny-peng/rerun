use rerun::external::re_renderer::{
    self,
    external::{smallvec::smallvec, wgpu},
    DrawPhase,
};

/// Implements a simple custom [`re_renderer::renderer::Renderer`] for drawing some shader defined 3D fractal.
pub struct FractalRenderer {
    render_pipeline: re_renderer::GpuRenderPipelineHandle,
}

/// GPU draw data for drawing fractal instances using [`FractalRenderer`].
#[derive(Clone)]
pub struct FractalDrawData;

impl re_renderer::renderer::DrawData for FractalDrawData {
    type Renderer = FractalRenderer;
}

impl FractalDrawData {
    pub fn new(ctx: &re_renderer::RenderContext) -> Self {
        let _ = ctx.renderer::<FractalRenderer>(); // TODO(andreas): This line ensures that the renderer exists. Currently this needs to be done ahead of time, but should be fully automatic!
        Self {}
    }
}

impl re_renderer::renderer::Renderer for FractalRenderer {
    type RendererDrawData = FractalDrawData;

    fn create_renderer(ctx: &re_renderer::RenderContext) -> Self {
        let shader_modules = &ctx.gpu_resources.shader_modules;
        let shader_module = shader_modules.get_or_create(
            ctx,
            &re_renderer::include_shader_module!("../shader/fractal.wgsl"),
        );

        let render_pipeline = ctx.gpu_resources.render_pipelines.get_or_create(
            ctx,
            &re_renderer::RenderPipelineDesc {
                label: "FractalRenderer::main".into(),
                pipeline_layout: ctx.gpu_resources.pipeline_layouts.get_or_create(
                    ctx,
                    &re_renderer::PipelineLayoutDesc {
                        label: "global only".into(),
                        entries: vec![ctx.global_bindings.layout],
                    },
                ),
                vertex_entrypoint: "vs_main".into(),
                vertex_handle: shader_module,
                fragment_entrypoint: "fs_main".into(),
                fragment_handle: shader_module,
                vertex_buffers: smallvec![],
                render_targets: smallvec![Some(
                    re_renderer::ViewBuilder::MAIN_TARGET_COLOR_FORMAT.into()
                )],
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: re_renderer::ViewBuilder::MAIN_TARGET_DEPTH_FORMAT,
                    depth_compare: wgpu::CompareFunction::Always,
                    depth_write_enabled: true, // writes some depth for testing
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: re_renderer::ViewBuilder::main_target_default_msaa_state(
                    ctx.render_config(),
                    false,
                ),
            },
        );

        Self { render_pipeline }
    }

    fn draw(
        &self,
        render_pipelines: &re_renderer::GpuRenderPipelinePoolAccessor<'_>,
        _phase: re_renderer::DrawPhase,
        pass: &mut wgpu::RenderPass<'_>,
        _draw_data: &FractalDrawData,
    ) -> Result<(), re_renderer::renderer::DrawError> {
        let pipeline = render_pipelines.get(self.render_pipeline)?;
        pass.set_pipeline(pipeline);
        pass.draw(0..3, 0..1);

        Ok(())
    }

    fn participated_phases() -> &'static [DrawPhase] {
        &[
            DrawPhase::Opaque,
            // TODO(andreas): Demonstrate how to render the outline layer.
            //DrawPhase::OutlineMask,
            // TODO(andreas): Demonstrate how to render the picking layer.
            //DrawPhase::PickingLayer,
        ]
    }
}
