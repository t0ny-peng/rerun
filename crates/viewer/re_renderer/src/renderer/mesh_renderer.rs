//! Mesh renderer.
//!
//! Uses instancing to render instances of the same mesh in a single draw call.
//! Instance data is kept in an instance-stepped vertex data.

use std::{collections::BTreeMap, sync::Arc};

use enumset::EnumSet;
use smallvec::smallvec;

use crate::{
    Color32, CpuWriteGpuReadError, DrawableCollector, OutlineMaskPreference, PickingLayerId,
    PickingLayerProcessor,
    draw_phases::{DrawPhase, OutlineMaskProcessor},
    include_shader_module,
    mesh::{GpuMesh, gpu_data::MaterialUniformBuffer, mesh_vertices},
    renderer::{DrawDataDrawable, DrawInstruction, DrawableCollectionViewInfo},
    view_builder::ViewBuilder,
    wgpu_resources::{
        BindGroupLayoutDesc, BufferDesc, GpuBindGroupLayoutHandle, GpuBuffer,
        GpuRenderPipelineHandle, GpuRenderPipelinePoolAccessor, PipelineLayoutDesc,
        RenderPipelineDesc,
    },
};

use super::{DrawData, DrawError, RenderContext, Renderer};

mod gpu_data {
    use ecolor::Color32;

    use crate::{mesh::mesh_vertices, wgpu_resources::VertexBufferLayout};

    /// Element in the gpu residing instance buffer.
    ///
    /// Keep in sync with `mesh_vertex.wgsl`
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct InstanceData {
        // Don't use aligned glam types because they enforce alignment.
        // (staging buffer might be 4 byte aligned only!)
        pub world_from_mesh_row_0: [f32; 4],
        pub world_from_mesh_row_1: [f32; 4],
        pub world_from_mesh_row_2: [f32; 4],

        pub world_from_mesh_normal_row_0: [f32; 3],
        pub world_from_mesh_normal_row_1: [f32; 3],
        pub world_from_mesh_normal_row_2: [f32; 3],

        pub additive_tint: Color32,

        pub picking_layer_id: [u32; 4],

        // Need only the first two bytes, but we want to keep everything aligned to at least 4 bytes.
        pub outline_mask_ids: [u8; 4],
    }

    impl InstanceData {
        pub fn vertex_buffer_layout() -> VertexBufferLayout {
            let shader_start_location = mesh_vertices::next_free_shader_location();

            VertexBufferLayout {
                array_stride: std::mem::size_of::<Self>() as _,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: VertexBufferLayout::attributes_from_formats(
                    shader_start_location,
                    [
                        // Affine mesh transform.
                        wgpu::VertexFormat::Float32x4,
                        wgpu::VertexFormat::Float32x4,
                        wgpu::VertexFormat::Float32x4,
                        // Transposed inverse mesh transform.
                        wgpu::VertexFormat::Float32x3,
                        wgpu::VertexFormat::Float32x3,
                        wgpu::VertexFormat::Float32x3,
                        // Tint color
                        wgpu::VertexFormat::Unorm8x4,
                        // Picking id.
                        // Again this adds overhead for non-picking passes, more this time. Consider moving this elsewhere.
                        wgpu::VertexFormat::Uint32x4,
                        // Outline mask.
                        // This adds a tiny bit of overhead to all instances during non-outline pass, but the alternative is having yet another vertex buffer.
                        wgpu::VertexFormat::Uint8x2,
                    ]
                    .into_iter(),
                ),
            }
        }
    }
}

/// A batch of mesh instances that are drawn together.
///
/// Note that we don't split the mesh by material.
#[derive(Clone)]
struct MeshBatch {
    mesh: Arc<GpuMesh>,

    instance_start_index: u32,
    instance_end_index: u32,

    /// Last index for meshes with with outlines
    ///
    /// We put all instances with outlines at the start of the instance buffer range, so this is always
    /// smaller or equal to `instance_end_index`.
    /// If it is equal to `instance_end_index`, there are no meshes with outlines in this batch.
    instance_end_index_with_outlines: u32,

    draw_phases: EnumSet<DrawPhase>,
}

#[derive(Clone)]
pub struct MeshDrawData {
    // There is a single instance buffer for all instances of all meshes.
    // This means we only ever need to bind the instance buffer once and then change the
    // instance range on every instanced draw call!
    instance_buffer: Option<GpuBuffer>,
    batches: Vec<MeshBatch>,
}

impl DrawData for MeshDrawData {
    type Renderer = MeshRenderer;

    fn collect_drawables(
        &self,
        _view_info: &DrawableCollectionViewInfo,
        collector: &mut DrawableCollector<'_>,
    ) {
        // TODO(andreas): transparency, distance sorting etc.

        for (batch_idx, batch) in self.batches.iter().enumerate() {
            collector.add_drawable(
                batch.draw_phases,
                DrawDataDrawable {
                    distance_sort_key: f32::MAX,
                    draw_data_payload: batch_idx as _,
                },
            );
        }
    }
}

pub struct GpuMeshInstance {
    /// Gpu mesh used by this instance
    pub gpu_mesh: Arc<GpuMesh>,

    /// Where this instance is placed in world space and how its oriented & scaled.
    pub world_from_mesh: glam::Affine3A,

    /// Per-instance (as opposed to per-material/mesh!) tint color that is added to the albedo texture.
    /// Alpha channel is currently unused.
    pub additive_tint: Color32,

    /// Optional outline mask setting for this instance.
    pub outline_mask_ids: OutlineMaskPreference,

    /// Picking layer id.
    pub picking_layer_id: PickingLayerId,
}

impl GpuMeshInstance {
    /// Creates a new instance of a mesh with all fields set to default except for required ones.
    pub fn new(gpu_mesh: Arc<GpuMesh>) -> Self {
        Self {
            gpu_mesh,
            world_from_mesh: glam::Affine3A::IDENTITY,
            additive_tint: Color32::TRANSPARENT,
            outline_mask_ids: OutlineMaskPreference::NONE,
            picking_layer_id: PickingLayerId::default(),
        }
    }
}

impl MeshDrawData {
    /// Transforms and uploads mesh instance data to be consumed by gpu.
    ///
    /// Tries bundling all mesh instances into a single draw data instance whenever possible.
    /// If you pass zero mesh instances, subsequent drawing will do nothing.
    /// Mesh data itself is gpu uploaded if not already present.
    pub fn new(
        ctx: &RenderContext,
        instances: &[GpuMeshInstance],
    ) -> Result<Self, CpuWriteGpuReadError> {
        re_tracing::profile_function!();

        if instances.is_empty() {
            return Ok(Self {
                batches: Vec::new(),
                instance_buffer: None,
            });
        }

        // Group by mesh to facilitate instancing.

        // TODO(andreas): Use a temp allocator
        let instance_buffer_size =
            (std::mem::size_of::<gpu_data::InstanceData>() * instances.len()) as _;
        let instance_buffer = ctx.gpu_resources.buffers.alloc(
            &ctx.device,
            &BufferDesc {
                label: "MeshDrawData::instance_buffer".into(),
                size: instance_buffer_size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );

        // NOTE: can't use HashMap here or we get undeterrministic rendering order.
        // See <https://github.com/rerun-io/rerun/issues/10116> for more.
        // Using a `BTreeMap` at least gives the same order every frame,
        // but since it uses the pointer address as the key,
        // it will still change if we run the app multiple times.
        let mut instances_by_mesh: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for instance in instances {
            instances_by_mesh
                // Use pointer equality, this is enough to determine if two instances use the same mesh.
                // (different mesh allocations have different gpu buffers internally, so they are by this definition not equal)
                .entry(Arc::as_ptr(&instance.gpu_mesh))
                .or_insert_with(|| Vec::with_capacity(instances.len()))
                .push(instance);
        }

        let mut batches = Vec::new();
        {
            let mut instance_buffer_staging = ctx
                .cpu_write_gpu_read_belt
                .lock()
                .allocate::<gpu_data::InstanceData>(
                &ctx.device,
                &ctx.gpu_resources.buffers,
                instances.len(),
            )?;

            let mut num_processed_instances = 0;
            for (_mesh_ptr, mut instances) in instances_by_mesh {
                let mut count = 0;
                let mut count_with_outlines = 0;

                // Put all instances with outlines at the start of the instance buffer range.
                instances.sort_by(|a, b| {
                    a.outline_mask_ids
                        .is_none()
                        .cmp(&b.outline_mask_ids.is_none())
                });

                let Some(first_instance) = instances.first() else {
                    continue;
                };
                let mesh = first_instance.gpu_mesh.clone();

                let any_material_has_transparency = mesh
                    .materials
                    .iter()
                    .any(|material| material.has_transparency);
                let all_materials_have_transparency = mesh
                    .materials
                    .iter()
                    .all(|material| material.has_transparency);

                for instance in instances {
                    count += 1;
                    count_with_outlines += instance.outline_mask_ids.is_some() as u32;

                    let world_from_mesh_mat3 = instance.world_from_mesh.matrix3;
                    // If the matrix is not invertible the draw result is likely invalid as well.
                    // However, at this point it's really hard to bail out!
                    // Also, by skipping drawing here, we'd make the result worse as there would be no mesh draw calls that could be debugged.
                    let world_from_mesh_normal =
                        if instance.world_from_mesh.matrix3.determinant() != 0.0 {
                            instance.world_from_mesh.matrix3.inverse().transpose()
                        } else {
                            glam::Mat3A::ZERO
                        };
                    instance_buffer_staging.push(gpu_data::InstanceData {
                        world_from_mesh_row_0: world_from_mesh_mat3
                            .row(0)
                            .extend(instance.world_from_mesh.translation.x)
                            .to_array(),
                        world_from_mesh_row_1: world_from_mesh_mat3
                            .row(1)
                            .extend(instance.world_from_mesh.translation.y)
                            .to_array(),
                        world_from_mesh_row_2: world_from_mesh_mat3
                            .row(2)
                            .extend(instance.world_from_mesh.translation.z)
                            .to_array(),
                        world_from_mesh_normal_row_0: world_from_mesh_normal.row(0).to_array(),
                        world_from_mesh_normal_row_1: world_from_mesh_normal.row(1).to_array(),
                        world_from_mesh_normal_row_2: world_from_mesh_normal.row(2).to_array(),
                        additive_tint: instance.additive_tint,
                        outline_mask_ids: instance
                            .outline_mask_ids
                            .0
                            .map_or([0, 0, 0, 0], |mask| [mask[0], mask[1], 0, 0]),
                        picking_layer_id: instance.picking_layer_id.into(),
                    })?;
                }

                let mut draw_phases = EnumSet::from(DrawPhase::PickingLayer);
                if count_with_outlines > 0 {
                    draw_phases |= DrawPhase::OutlineMask;
                }
                if any_material_has_transparency {
                    draw_phases |= DrawPhase::Transparent;
                }
                if !all_materials_have_transparency {
                    draw_phases |= DrawPhase::Opaque;
                }

                // TODO: don't batch meshes with transparency as aggressively.
                // I think it's fine to "acidentially" aggresssively de-batch mixed meshes.

                batches.push(MeshBatch {
                    mesh,
                    instance_start_index: num_processed_instances,
                    instance_end_index: (num_processed_instances + count),
                    instance_end_index_with_outlines: (num_processed_instances
                        + count_with_outlines),
                    draw_phases,
                });

                num_processed_instances += count;
            }
            assert_eq!(num_processed_instances as usize, instances.len());
            instance_buffer_staging.copy_to_buffer(
                ctx.active_frame.before_view_builder_encoder.lock().get(),
                &instance_buffer,
                0,
            )?;
        }

        Ok(Self {
            batches,
            instance_buffer: Some(instance_buffer),
        })
    }
}

pub struct MeshRenderer {
    rp_shaded: GpuRenderPipelineHandle,

    rp_shaded_alpha_blended_cull_back: GpuRenderPipelineHandle,
    rp_shaded_alpha_blended_cull_front: GpuRenderPipelineHandle,

    rp_picking_layer: GpuRenderPipelineHandle,
    rp_outline_mask: GpuRenderPipelineHandle,
    pub bind_group_layout: GpuBindGroupLayoutHandle,
}

impl Renderer for MeshRenderer {
    type RendererDrawData = MeshDrawData;

    fn create_renderer(ctx: &RenderContext) -> Self {
        re_tracing::profile_function!();

        let render_pipelines = &ctx.gpu_resources.render_pipelines;

        let bind_group_layout = ctx.gpu_resources.bind_group_layouts.get_or_create(
            &ctx.device,
            &BindGroupLayoutDesc {
                label: "MeshRenderer::bind_group_layout".into(),
                entries: vec![
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: (std::mem::size_of::<MaterialUniformBuffer>() as u64)
                                .try_into()
                                .ok(),
                        },
                        count: None,
                    },
                ],
            },
        );
        let pipeline_layout = ctx.gpu_resources.pipeline_layouts.get_or_create(
            ctx,
            &PipelineLayoutDesc {
                label: "MeshRenderer::pipeline_layout".into(),
                entries: vec![ctx.global_bindings.layout, bind_group_layout],
            },
        );

        let shader_module = ctx.gpu_resources.shader_modules.get_or_create(
            ctx,
            &include_shader_module!("../../shader/instanced_mesh.wgsl"),
        );

        // TODO(andreas): Make this configurable.
        // Use GLTF convention right now.
        let front_face = wgpu::FrontFace::Ccw;

        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None, //Some(wgpu::Face::Back), // TODO(andreas): Need to specify from outside if mesh is CW or CCW?
            front_face,
            ..Default::default()
        };
        // Put instance vertex buffer on slot 0 since it doesn't change for several draws.
        let vertex_buffers: smallvec::SmallVec<[_; 4]> =
            std::iter::once(gpu_data::InstanceData::vertex_buffer_layout())
                .chain(mesh_vertices::vertex_buffer_layouts())
                .collect();

        let rp_shaded_desc = RenderPipelineDesc {
            label: "MeshRenderer::rp_shaded".into(),
            pipeline_layout,
            vertex_entrypoint: "vs_main".into(),
            vertex_handle: shader_module,
            fragment_entrypoint: "fs_main_shaded".into(),
            fragment_handle: shader_module,
            vertex_buffers,
            render_targets: smallvec![Some(ViewBuilder::MAIN_TARGET_COLOR_FORMAT.into())],
            primitive,
            depth_stencil: Some(ViewBuilder::MAIN_TARGET_DEFAULT_DEPTH_STATE),
            multisample: ViewBuilder::main_target_default_msaa_state(ctx.render_config(), false),
        };
        let rp_shaded = render_pipelines.get_or_create(ctx, &rp_shaded_desc);

        let rp_shaded_alpha_blended_cull_back_desc = RenderPipelineDesc {
            label: "MeshRenderer::rp_shaded_alpha_blended_front".into(),
            render_targets: smallvec![Some(wgpu::ColorTargetState {
                format: ViewBuilder::MAIN_TARGET_COLOR_FORMAT,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            depth_stencil: Some(ViewBuilder::MAIN_TARGET_DEFAULT_DEPTH_STATE_NO_WRITE),
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Back),
                front_face,
                ..primitive
            },
            ..rp_shaded_desc.clone()
        };
        let rp_shaded_alpha_blended_cull_front_desc = RenderPipelineDesc {
            label: "MeshRenderer::rp_shaded_alpha_blended_back".into(),
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Front),
                ..primitive
            },
            ..rp_shaded_alpha_blended_cull_back_desc.clone()
        };
        let rp_shaded_alpha_blended_cull_back =
            render_pipelines.get_or_create(ctx, &rp_shaded_alpha_blended_cull_back_desc);
        let rp_shaded_alpha_blended_cull_front =
            render_pipelines.get_or_create(ctx, &rp_shaded_alpha_blended_cull_front_desc);

        let rp_picking_layer = render_pipelines.get_or_create(
            ctx,
            &RenderPipelineDesc {
                label: "MeshRenderer::rp_picking_layer".into(),
                fragment_entrypoint: "fs_main_picking_layer".into(),
                render_targets: smallvec![Some(PickingLayerProcessor::PICKING_LAYER_FORMAT.into())],
                depth_stencil: PickingLayerProcessor::PICKING_LAYER_DEPTH_STATE,
                multisample: PickingLayerProcessor::PICKING_LAYER_MSAA_STATE,
                ..rp_shaded_desc.clone()
            },
        );
        let rp_outline_mask = render_pipelines.get_or_create(
            ctx,
            &RenderPipelineDesc {
                label: "MeshRenderer::rp_outline_mask".into(),
                fragment_entrypoint: "fs_main_outline_mask".into(),
                render_targets: smallvec![Some(OutlineMaskProcessor::MASK_FORMAT.into())],
                depth_stencil: OutlineMaskProcessor::MASK_DEPTH_STATE,
                multisample: OutlineMaskProcessor::mask_default_msaa_state(ctx.device_caps().tier),
                ..rp_shaded_desc
            },
        );

        Self {
            rp_shaded,
            rp_shaded_alpha_blended_cull_back,
            rp_shaded_alpha_blended_cull_front,
            rp_picking_layer,
            rp_outline_mask,
            bind_group_layout,
        }
    }

    fn draw(
        &self,
        render_pipelines: &GpuRenderPipelinePoolAccessor<'_>,
        phase: DrawPhase,
        pass: &mut wgpu::RenderPass<'_>,
        draw_instructions: &[DrawInstruction<'_, Self::RendererDrawData>],
    ) -> Result<(), DrawError> {
        re_tracing::profile_function!();

        let pipeline_handle = match phase {
            DrawPhase::OutlineMask => Some(self.rp_outline_mask),
            DrawPhase::Opaque => Some(self.rp_shaded),
            DrawPhase::PickingLayer => Some(self.rp_picking_layer),
            DrawPhase::Transparent => None, // Handled later since we have to switch back and forth between front & back face culling.
            _ => unreachable!("We were called on a phase we weren't subscribed to: {phase:?}"),
        };
        if let Some(pipeline_handle) = pipeline_handle {
            pass.set_pipeline(render_pipelines.get(pipeline_handle)?);
        }

        // TODO(andreas): use drawables to orchestrate drawing.
        for DrawInstruction {
            draw_data,
            drawables,
        } in draw_instructions
        {
            let Some(instance_buffer) = &draw_data.instance_buffer else {
                continue; // Instance buffer was empty.
            };
            pass.set_vertex_buffer(0, instance_buffer.slice(..));

            for drawable in *drawables {
                let mesh_batch = &draw_data.batches[drawable.draw_data_payload as usize];

                let vertex_buffer_combined = &mesh_batch.mesh.vertex_buffer_combined;
                let index_buffer = &mesh_batch.mesh.index_buffer;

                pass.set_vertex_buffer(
                    1,
                    vertex_buffer_combined
                        .slice(mesh_batch.mesh.vertex_buffer_positions_range.clone()),
                );
                pass.set_vertex_buffer(
                    2,
                    vertex_buffer_combined
                        .slice(mesh_batch.mesh.vertex_buffer_colors_range.clone()),
                );
                pass.set_vertex_buffer(
                    3,
                    vertex_buffer_combined
                        .slice(mesh_batch.mesh.vertex_buffer_normals_range.clone()),
                );
                pass.set_vertex_buffer(
                    4,
                    vertex_buffer_combined
                        .slice(mesh_batch.mesh.vertex_buffer_texcoord_range.clone()),
                );
                pass.set_index_buffer(
                    index_buffer.slice(mesh_batch.mesh.index_buffer_range.clone()),
                    wgpu::IndexFormat::Uint32,
                );

                let instance_range = if phase == DrawPhase::OutlineMask {
                    mesh_batch.instance_start_index..mesh_batch.instance_end_index_with_outlines
                } else {
                    mesh_batch.instance_start_index..mesh_batch.instance_end_index
                };
                debug_assert!(!instance_range.is_empty());

                for material in &mesh_batch.mesh.materials {
                    if phase == DrawPhase::Transparent && !material.has_transparency {
                        continue;
                    }
                    if phase == DrawPhase::Opaque && material.has_transparency {
                        continue;
                    }

                    pass.set_bind_group(1, &material.bind_group, &[]);

                    if phase == DrawPhase::Transparent {
                        // First draw without front faces.
                        pass.set_pipeline(
                            render_pipelines.get(self.rp_shaded_alpha_blended_cull_front)?,
                        );
                        pass.draw_indexed(material.index_range.clone(), 0, instance_range.clone());

                        // And then without back faces.
                        pass.set_pipeline(
                            render_pipelines.get(self.rp_shaded_alpha_blended_cull_back)?,
                        );
                        pass.draw_indexed(material.index_range.clone(), 0, instance_range.clone());
                    } else {
                        pass.draw_indexed(material.index_range.clone(), 0, instance_range.clone());
                    }
                }
            }
        }

        Ok(())
    }
}
