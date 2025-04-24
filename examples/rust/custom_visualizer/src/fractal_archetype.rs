use rerun::{external::re_types::try_serialize_field, Component};

/// Custom archetype for drawing a simple raymarched fractal in the 3D view.
#[derive(Default)]
pub struct Fractal {
    pub positions: Option<rerun::SerializedComponentBatch>,
    pub colors: Option<rerun::SerializedComponentBatch>,
}

impl rerun::Archetype for Fractal {
    type Indicator = rerun::GenericIndicatorComponent<Self>;

    fn indicator() -> rerun::SerializedComponentBatch {
        use rerun::ComponentBatch as _;
        #[allow(clippy::unwrap_used)]
        Self::Indicator::default().serialized().unwrap()
    }

    fn name() -> rerun::ArchetypeName {
        "CustomFractal".into()
    }

    fn display_name() -> &'static str {
        "Fractal"
    }

    fn required_components() -> ::std::borrow::Cow<'static, [rerun::ComponentDescriptor]> {
        vec![Self::descriptor_positions()].into()
    }
}

impl Fractal {
    /// Returns the [`rerun::ComponentDescriptor`] for [`Self::positions`].
    #[inline]
    pub fn descriptor_positions() -> rerun::ComponentDescriptor {
        rerun::ComponentDescriptor {
            archetype_name: Some("CustomFractal".into()),
            component_name: rerun::Position3D::name(),
            archetype_field_name: Some("positions".into()),
        }
    }

    /// Returns the [`rerun::ComponentDescriptor`] for [`Self::colors`].
    #[inline]
    pub fn descriptor_colors() -> rerun::ComponentDescriptor {
        rerun::ComponentDescriptor {
            archetype_name: Some("CustomFractal".into()),
            component_name: rerun::Color::name(),
            archetype_field_name: Some("colors".into()),
        }
    }

    #[inline]
    pub fn new(
        positions: impl IntoIterator<Item = impl Into<rerun::components::Position3D>>,
    ) -> Self {
        Self::default().with_positions(positions)
    }

    #[inline]
    pub fn with_positions(
        mut self,
        positions: impl IntoIterator<Item = impl Into<rerun::components::Position3D>>,
    ) -> Self {
        self.positions = try_serialize_field(Self::descriptor_positions(), positions);
        self
    }

    #[inline]
    pub fn with_colors(
        mut self,
        vertex_colors: impl IntoIterator<Item = impl Into<rerun::components::Color>>,
    ) -> Self {
        self.colors = try_serialize_field(Self::descriptor_colors(), vertex_colors);
        self
    }
}

impl rerun::AsComponents for Fractal {
    #[inline]
    fn as_serialized_batches(&self) -> Vec<rerun::SerializedComponentBatch> {
        use rerun::Archetype as _;
        [
            Some(Self::indicator()),
            self.positions.clone(),
            self.colors.clone(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}
