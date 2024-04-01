use emath::Pos2;
use enumset::EnumSet;
use std::ops::{Add, AddAssign, Sub};

use crate::config::{GizmoConfig, GizmoDirection, GizmoMode, PreparedGizmoConfig};
use crate::math::screen_to_world;
use epaint::Mesh;
use glam::{DMat4, DQuat, DVec3};

use crate::subgizmo::rotation::RotationParams;
use crate::subgizmo::scale::ScaleParams;
use crate::subgizmo::translation::TranslationParams;
use crate::subgizmo::{
    common::TransformKind, ArcballSubGizmo, RotationSubGizmo, ScaleSubGizmo, SubGizmo,
    SubGizmoControl, TranslationSubGizmo,
};

pub struct Gizmo {
    config: PreparedGizmoConfig,
    last_modes: EnumSet<GizmoMode>,
    subgizmos: Vec<SubGizmo>,
    active_subgizmo_id: Option<u64>,

    target_start_transforms: Vec<DMat4>,
}

impl Default for Gizmo {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl Gizmo {
    pub fn new(config: GizmoConfig) -> Self {
        Self {
            config: PreparedGizmoConfig::from_config(config),
            last_modes: Default::default(),
            subgizmos: Default::default(),
            active_subgizmo_id: None,

            target_start_transforms: vec![],
        }
    }

    /// Current configuration used by the gizmo.
    pub fn config(&self) -> &GizmoConfig {
        &self.config
    }

    /// Update the configuration used by the gizmo.
    pub fn update_config(&mut self, config: GizmoConfig) {
        self.config = PreparedGizmoConfig::from_config(config);
    }

    /// Were any of the subgizmoes focused after latest [`Gizmo::update`] call.
    pub fn is_any_focused(&self) -> bool {
        self.subgizmos.iter().any(|subgizmo| subgizmo.is_focused())
    }

    /// Updates the gizmo based on given interaction information.
    ///
    /// Returns the result of the interaction with the updated transformation.
    ///
    /// [`Some`] is returned when any of the subgizmos is being dragged, [`None`] otherwise.
    pub fn update(
        &mut self,
        interaction: GizmoInteraction,
        targets: impl Iterator<Item = mint::RowMatrix4<f64>>,
    ) -> Option<GizmoResult> {
        if !self.config.viewport.is_finite() {
            return None;
        }

        // Mode was changed. Update all subgizmos accordingly.
        if self.config.modes != self.last_modes {
            self.last_modes = self.config.modes;

            self.subgizmos.clear();

            // Choose subgizmos based on the gizmo mode
            for mode in self.config.modes {
                match mode {
                    GizmoMode::Rotate => {
                        self.add_rotation();
                    }
                    GizmoMode::Translate => {
                        self.add_translation();
                    }
                    GizmoMode::Scale => {
                        self.add_scale();
                    }
                };
            }
        }

        let targets = targets.map(DMat4::from).collect::<Vec<_>>();

        // Update the gizmo based on the given targets.
        self.config.update_for_targets(&targets);

        for subgizmo in &mut self.subgizmos {
            // Update current configuration to each subgizmo.
            subgizmo.update_config(self.config);
            // All subgizmoes are initially considered unfocused.
            subgizmo.set_focused(false);
        }

        let mut result = None;

        let pointer_ray = self.pointer_ray(Pos2::from(interaction.cursor_pos));

        // If there is no active subgizmo, find which one of them
        // is under the mouse pointer, if any.
        if self.active_subgizmo_id.is_none() {
            if let Some(subgizmo) = self.pick_subgizmo(pointer_ray) {
                subgizmo.set_focused(true);

                // If we started dragging from one of the subgizmos, mark it as active.
                if interaction.drag_started {
                    self.active_subgizmo_id = Some(subgizmo.id());
                    self.target_start_transforms = targets.clone();
                }
            }
        }

        let mut active_subgizmo = self.active_subgizmo_id.and_then(|id| {
            self.subgizmos
                .iter_mut()
                .find(|subgizmo| subgizmo.id() == id)
        });

        if let Some(subgizmo) = active_subgizmo.as_mut() {
            if interaction.dragging {
                subgizmo.set_active(true);
                subgizmo.set_focused(true);
                result = subgizmo.update(pointer_ray);
            } else {
                subgizmo.set_active(false);
                subgizmo.set_focused(false);
                self.active_subgizmo_id = None;
            }
        }

        if let Some((_, result)) = active_subgizmo.zip(result.as_mut()) {
            for (target_start_transform, target_transform) in
                self.target_start_transforms.iter().zip(targets)
            {
                let mut new_target_transform = target_transform;

                match result.mode {
                    GizmoMode::Rotate => {
                        // Rotate around the target group origin

                        let group_translation = DMat4::from_translation(self.config.translation);

                        new_target_transform =
                            group_translation.inverse().mul_mat4(&new_target_transform);

                        new_target_transform = DMat4::from_quat(result.rotation.into())
                            .mul_mat4(&new_target_transform);

                        new_target_transform = group_translation.mul_mat4(&new_target_transform);
                    }
                    GizmoMode::Translate => {
                        new_target_transform = DMat4::from_translation(result.translation.into())
                            .mul_mat4(&new_target_transform);
                    }
                    GizmoMode::Scale => {
                        let (start_scale, _, _) =
                            target_start_transform.to_scale_rotation_translation();

                        let (_, target_rotation, target_translation) =
                            target_transform.to_scale_rotation_translation();

                        new_target_transform = DMat4::from_scale_rotation_translation(
                            start_scale * DVec3::from(result.scale),
                            target_rotation,
                            target_translation,
                        );
                    }
                }

                result.targets.push(new_target_transform.into());
            }
        }

        result
    }

    /// Return all the necessary data to draw the latest gizmo interaction.
    ///
    /// The gizmo draw data consists of vertices in viewport coordinates.
    pub fn draw(&self) -> GizmoDrawData {
        let mut draw_data = GizmoDrawData::default();
        for subgizmo in &self.subgizmos {
            if self.active_subgizmo_id.is_none() || subgizmo.is_active() {
                draw_data += subgizmo.draw();
            }
        }

        draw_data
    }

    /// Picks the subgizmo that is closest to the given world space ray.
    fn pick_subgizmo(&mut self, ray: Ray) -> Option<&mut SubGizmo> {
        self.subgizmos
            .iter_mut()
            .filter_map(|subgizmo| subgizmo.pick(ray).map(|t| (t, subgizmo)))
            .min_by(|(first, _), (second, _)| {
                first
                    .partial_cmp(second)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(_, subgizmo)| subgizmo)
    }

    /// Adds rotation subgizmos
    fn add_rotation(&mut self) {
        self.subgizmos.extend([
            RotationSubGizmo::new(
                self.config,
                RotationParams {
                    direction: GizmoDirection::X,
                },
            )
            .into(),
            RotationSubGizmo::new(
                self.config,
                RotationParams {
                    direction: GizmoDirection::Y,
                },
            )
            .into(),
            RotationSubGizmo::new(
                self.config,
                RotationParams {
                    direction: GizmoDirection::Z,
                },
            )
            .into(),
            RotationSubGizmo::new(
                self.config,
                RotationParams {
                    direction: GizmoDirection::View,
                },
            )
            .into(),
        ]);

        self.subgizmos
            .push(ArcballSubGizmo::new(self.config, ()).into());
    }

    /// Adds translation subgizmos
    fn add_translation(&mut self) {
        self.subgizmos.extend([
            TranslationSubGizmo::new(
                self.config,
                TranslationParams {
                    direction: GizmoDirection::X,
                    transform_kind: TransformKind::Axis,
                },
            )
            .into(),
            TranslationSubGizmo::new(
                self.config,
                TranslationParams {
                    direction: GizmoDirection::Y,
                    transform_kind: TransformKind::Axis,
                },
            )
            .into(),
            TranslationSubGizmo::new(
                self.config,
                TranslationParams {
                    direction: GizmoDirection::Z,
                    transform_kind: TransformKind::Axis,
                },
            )
            .into(),
            TranslationSubGizmo::new(
                self.config,
                TranslationParams {
                    direction: GizmoDirection::View,
                    transform_kind: TransformKind::Plane,
                },
            )
            .into(),
        ]);

        // Plane subgizmos are not added when both translation and scaling are enabled.
        if !self.config.modes.contains(GizmoMode::Scale) {
            self.subgizmos.extend([
                TranslationSubGizmo::new(
                    self.config,
                    TranslationParams {
                        direction: GizmoDirection::X,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
                TranslationSubGizmo::new(
                    self.config,
                    TranslationParams {
                        direction: GizmoDirection::Y,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
                TranslationSubGizmo::new(
                    self.config,
                    TranslationParams {
                        direction: GizmoDirection::Z,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
            ]);
        }
    }

    /// Adds scale subgizmos
    fn add_scale(&mut self) {
        self.subgizmos.extend([
            ScaleSubGizmo::new(
                self.config,
                ScaleParams {
                    direction: GizmoDirection::X,
                    transform_kind: TransformKind::Axis,
                },
            )
            .into(),
            ScaleSubGizmo::new(
                self.config,
                ScaleParams {
                    direction: GizmoDirection::Y,
                    transform_kind: TransformKind::Axis,
                },
            )
            .into(),
            ScaleSubGizmo::new(
                self.config,
                ScaleParams {
                    direction: GizmoDirection::Z,
                    transform_kind: TransformKind::Axis,
                },
            )
            .into(),
        ]);

        // Uniform scaling subgizmo is added when only scaling is enabled.
        // Otherwise it would overlap with rotation or translation subgizmos.
        if self.config.modes.len() == 1 {
            self.subgizmos.push(
                ScaleSubGizmo::new(
                    self.config,
                    ScaleParams {
                        direction: GizmoDirection::View,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
            );
        }

        // Plane subgizmos are not added when both translation and scaling are enabled.
        if !self.config.modes.contains(GizmoMode::Translate) {
            self.subgizmos.extend([
                ScaleSubGizmo::new(
                    self.config,
                    ScaleParams {
                        direction: GizmoDirection::X,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
                ScaleSubGizmo::new(
                    self.config,
                    ScaleParams {
                        direction: GizmoDirection::Y,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
                ScaleSubGizmo::new(
                    self.config,
                    ScaleParams {
                        direction: GizmoDirection::Z,
                        transform_kind: TransformKind::Plane,
                    },
                )
                .into(),
            ]);
        }
    }

    /// Calculate a world space ray from given screen space position
    fn pointer_ray(&self, screen_pos: Pos2) -> Ray {
        let mat = self.config.view_projection.inverse();
        let origin = screen_to_world(self.config.viewport, mat, screen_pos, -1.0);
        let target = screen_to_world(self.config.viewport, mat, screen_pos, 1.0);

        let direction = target.sub(origin).normalize();

        Ray {
            screen_pos,
            origin,
            direction,
        }
    }
}

/// Information needed for interacting with the gizmo.
#[derive(Default, Clone, Copy, Debug)]
pub struct GizmoInteraction {
    /// Current cursor position in window coordinates.
    pub cursor_pos: (f32, f32),
    /// Whether dragging was started this frame.
    /// Usually this is set to true if the primary mouse
    /// button was just pressed.
    pub drag_started: bool,
    /// Whether the user is currently dragging.
    /// Usually this is set to true whenever the primary mouse
    /// button is being pressed.
    pub dragging: bool,
}

/// Result of a gizmo transformation
#[derive(Debug, Clone)]
pub struct GizmoResult {
    /// Updated scale
    pub scale: mint::Vector3<f64>,
    /// Updated rotation
    pub rotation: mint::Quaternion<f64>,
    /// Updated translation
    pub translation: mint::Vector3<f64>,
    /// Mode of the active subgizmo
    pub mode: GizmoMode,
    /// Total scale, rotation or translation of the current gizmo activation, depending on mode
    pub value: Option<[f64; 3]>,

    pub targets: Vec<mint::RowMatrix4<f64>>,
}

impl Default for GizmoResult {
    fn default() -> Self {
        Self {
            scale: DVec3::ONE.into(),
            rotation: DQuat::IDENTITY.into(),
            translation: DVec3::ZERO.into(),
            mode: GizmoMode::Rotate,
            value: None,
            targets: vec![],
        }
    }
}

impl GizmoResult {
    /// Updated transformation matrix in column major order.
    pub fn transform(&self) -> mint::ColumnMatrix4<f64> {
        DMat4::from_scale_rotation_translation(
            self.scale.into(),
            self.rotation.into(),
            self.translation.into(),
        )
        .into()
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Ray {
    pub screen_pos: Pos2,
    pub origin: DVec3,
    pub direction: DVec3,
}

#[derive(Default, Clone, Debug)]
pub struct GizmoDrawData {
    pub vertices: Vec<[f32; 2]>,
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
}

impl From<Mesh> for GizmoDrawData {
    fn from(mesh: Mesh) -> Self {
        let (vertices, colors): (Vec<_>, Vec<_>) = mesh
            .vertices
            .iter()
            .map(|vertex| {
                (
                    [vertex.pos.x, vertex.pos.y],
                    vertex.color.to_normalized_gamma_f32(),
                )
            })
            .unzip();

        Self {
            vertices,
            colors,
            indices: mesh.indices,
        }
    }
}

impl AddAssign for GizmoDrawData {
    fn add_assign(&mut self, rhs: Self) {
        let index_offset = self.vertices.len() as u32;
        self.vertices.extend(rhs.vertices);
        self.colors.extend(rhs.colors);
        self.indices
            .extend(rhs.indices.into_iter().map(|idx| index_offset + idx));
    }
}

impl Add for GizmoDrawData {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}
