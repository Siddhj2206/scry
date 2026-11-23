//! Virtual output management.
//!
//! scry has no physical displays: every `wl_output` is a software-defined
//! [`OutputSpec`] applied through
//! [`CompositorHandle::update_output`](crate::CompositorHandle::update_output).
//! This module owns the registry translating specs into smithay `Output`s,
//! keeping `wl_output`/`zxdg_output_v1` advertisement, [`Space`] placement and
//! fractional-scale bookkeeping in one place.

use std::collections::HashMap;

use smithay::{
    desktop::{Space, Window},
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{
        DisplayHandle, backend::GlobalId, protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Transform},
    wayland::{
        compositor::{SurfaceData, send_surface_state},
        fractional_scale::with_fractional_scale,
    },
};

use crate::{error::Error, event::Rect};

/// The output every compositor starts with: `Virtual-1`, 1920x1080 @ 60Hz,
/// scale 1 at the space origin.
pub(crate) const DEFAULT_OUTPUT: (&str, (i32, i32), i32, f64, (i32, i32)) =
    ("Virtual-1", (1920, 1080), 60_000, 1.0, (0, 0));

/// Configuration of a virtual `wl_output`, supplied by callers.
///
/// `size` is the advertised mode's physical pixel size; the output's logical
/// size inside the compositor [`Space`] is `size / scale`. A 3840x2160 mode
/// with `scale: 2.0` therefore presents a 1920x1080 logical output to clients.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSpec {
    /// `wl_output.name`; also the stable key identifying the output for
    /// updates and removal.
    pub name: String,
    /// Physical mode size in pixels.
    pub size: (i32, i32),
    /// Refresh rate in millihertz (`60_000` = 60Hz).
    pub refresh_millihertz: i32,
    /// Advertised scale factor. Fractional values are honored: `wl_output`
    /// reports the rounded-up integer while `wp_fractional_scale_v1` and
    /// `zxdg_output_v1` carry the exact value.
    pub scale: f64,
    /// Logical position of the output's top-left corner in compositor space.
    /// Outputs may overlap; a window on the overlap sees the highest scale.
    pub position: (i32, i32),
}

/// A snapshot of a virtual output's current configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// `wl_output.name`.
    pub name: String,
    /// Physical mode size in pixels.
    pub size: (i32, i32),
    /// Refresh rate in millihertz.
    pub refresh_millihertz: i32,
    /// The exact advertised scale factor.
    pub scale: f64,
    /// The output's logical geometry inside compositor space.
    pub geometry: Rect,
}

/// Validates `spec`, returning a normalized copy or an error.
fn validate(spec: OutputSpec) -> Result<OutputSpec, Error> {
    if spec.name.is_empty() {
        return Err(Error::InvalidOutputSpec("name must not be empty".into()));
    }
    if spec.size.0 <= 0 || spec.size.1 <= 0 {
        return Err(Error::InvalidOutputSpec(format!(
            "mode size must be positive, got {}x{}",
            spec.size.0, spec.size.1
        )));
    }
    if spec.refresh_millihertz <= 0 {
        return Err(Error::InvalidOutputSpec(format!(
            "refresh must be positive, got {}",
            spec.refresh_millihertz
        )));
    }
    if !spec.scale.is_finite() || spec.scale <= 0.0 {
        return Err(Error::InvalidOutputSpec(format!(
            "scale must be finite and positive, got {}",
            spec.scale
        )));
    }
    Ok(spec)
}

/// The registry of virtual outputs, keyed by `wl_output.name`.
///
/// Insertion order is preserved in [`Self::iter`] so `outputs()` listings and
/// "primary output" lookups are deterministic.
pub(crate) struct VirtualOutputs {
    outputs: HashMap<String, (Output, GlobalId)>,
    order: Vec<String>,
}

impl VirtualOutputs {
    /// An empty registry; [`Self::update`] adds outputs.
    pub(crate) fn new() -> Self {
        Self {
            outputs: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Adds or reconfigures a virtual output from `spec`.
    ///
    /// New names create an `Output`, advertise its `wl_output` global and map
    /// it into `space`; existing names push `mode`/`scale`/`geometry`/`done`
    /// to every bound `wl_output` and move the output's logical placement.
    pub(crate) fn update(
        &mut self,
        dh: &DisplayHandle,
        space: &mut Space<Window>,
        spec: OutputSpec,
    ) -> Result<(), Error> {
        let spec = validate(spec)?;
        let mode = Mode {
            size: spec.size.into(),
            refresh: spec.refresh_millihertz,
        };
        let position: Point<i32, Logical> = spec.position.into();
        let scale = Scale::Fractional(spec.scale);

        if let Some((output, _global)) = self.outputs.get(&spec.name) {
            output.change_current_state(
                Some(mode),
                Some(Transform::Normal),
                Some(scale),
                Some(position),
            );
            output.set_preferred(mode);
            space.map_output(output, position);
            return Ok(());
        }

        let output = Output::new(
            spec.name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "scry".to_string(),
                model: "virtual".to_string(),
            },
        );
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(scale),
            Some(position),
        );
        output.set_preferred(mode);
        let global = output.create_global::<crate::state::ScryState>(dh);
        space.map_output(&output, position);
        self.order.push(spec.name.clone());
        self.outputs.insert(spec.name, (output, global));
        Ok(())
    }

    /// Removes and unadvertises the output named `name`.
    ///
    /// Refuses to remove the last remaining output: surfaces would lose their
    /// scale reference and frame callbacks their scanout anchor.
    pub(crate) fn remove(
        &mut self,
        dh: &DisplayHandle,
        space: &mut Space<Window>,
        name: &str,
    ) -> Result<(), Error> {
        let Some((output, global)) = self.outputs.get(name) else {
            return Err(Error::UnknownOutput(name.to_string()));
        };
        if self.outputs.len() == 1 {
            return Err(Error::InvalidOutputSpec(
                "cannot remove the last remaining output".into(),
            ));
        }
        space.unmap_output(output);
        dh.remove_global::<crate::state::ScryState>(global.clone());
        let (output, _global) = self.outputs.remove(name).unwrap();
        drop(output);
        self.order.retain(|n| n != name);
        Ok(())
    }

    /// The output named `name`, if present.
    pub(crate) fn get(&self, name: &str) -> Option<&Output> {
        self.outputs.get(name).map(|(output, _)| output)
    }

    /// The first-registered (primary) output, used as the scanout anchor for
    /// `wl_surface.frame` callbacks.
    pub(crate) fn primary(&self) -> Option<&Output> {
        self.order
            .first()
            .and_then(|name| self.outputs.get(name))
            .map(|(output, _)| output)
    }

    /// All outputs in registration order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Output> {
        self.order
            .iter()
            .filter_map(|name| self.outputs.get(name).map(|(output, _)| output))
    }

    /// External metadata snapshots of every registered output.
    pub(crate) fn info(&self, space: &Space<Window>) -> Vec<OutputInfo> {
        self.iter()
            .map(|output| {
                let (size, refresh_millihertz) = output
                    .current_mode()
                    .map(|mode| ((mode.size.w, mode.size.h), mode.refresh))
                    .unwrap_or(((0, 0), 0));
                OutputInfo {
                    name: output.name(),
                    size,
                    refresh_millihertz,
                    scale: output.current_scale().fractional_scale(),
                    geometry: space
                        .output_geometry(output)
                        .map(Rect::from)
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// The scale to advertise to a window's surfaces: the highest fractional
    /// scale of the outputs it overlaps, falling back to the primary
    /// output's scale when it overlaps none.
    pub(crate) fn preferred_scale_for(&self, space: &Space<Window>, window: &Window) -> f64 {
        space
            .outputs_for_element(window)
            .iter()
            .map(|output| output.current_scale().fractional_scale())
            .fold(None, |acc: Option<f64>, scale| {
                Some(acc.map_or(scale, |a| a.max(scale)))
            })
            .unwrap_or_else(|| self.primary_scale())
    }

    /// The primary output's fractional scale, or `1.0` if none exists.
    pub(crate) fn primary_scale(&self) -> f64 {
        self.primary()
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }
}

/// Publishes `scale` to `surface`'s protocol-level scale hints: the
/// `wp_fractional_scale_v1` preferred scale (n/120 units) and the
/// `wl_surface` v6 `preferred_buffer_scale` (rounded up).
pub(crate) fn apply_scale_hints(surface: &WlSurface, states: &SurfaceData, scale: f64) {
    with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
    send_surface_state(surface, states, scale.ceil() as i32, Transform::Normal);
}
