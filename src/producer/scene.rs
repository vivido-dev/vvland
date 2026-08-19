use std::io;

use vivid_protocol::cbor::Value;
use vivid_protocol::scene::SceneNode;

/// Grid metrics of the `terminal-surface-v1` target, extracted from the target descriptor.
///
/// Vivid 1.5 carries these on the negotiated target instead of a producer-side display state;
/// the pipeline reads them from the session and the terminal placement math stays here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalDisplay {
    pub grid_columns: u32,
    pub grid_rows: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

/// Grid-cell coordinate space of the terminal target.
pub const COORDINATE_SPACE_GRID_CELL: u64 = 1;
/// The text layer between the target's background and glyph layers.
pub const TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH: u64 = 1;

const FIXED_ONE: i128 = 1_i128 << 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub source_width: u32,
    pub source_height: u32,
    status_row: u32,
}

impl Placement {
    pub fn calculate(
        display: TerminalDisplay,
        source_width: u32,
        source_height: u32,
    ) -> io::Result<Self> {
        Self::calculate_mode(display, source_width, source_height, true)
    }

    fn calculate_mode(
        display: TerminalDisplay,
        source_width: u32,
        source_height: u32,
        reserve_status_row: bool,
    ) -> io::Result<Self> {
        let reserved_rows = if reserve_status_row { 1 } else { 0 };
        if source_width == 0
            || source_height == 0
            || display.grid_columns == 0
            || display.grid_rows < 1 + reserved_rows
            || display.cell_width == 0
            || display.cell_height == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "display or source geometry cannot reserve a streaming area",
            ));
        }
        let available_columns = i128::from(display.grid_columns);
        let available_rows = i128::from(display.grid_rows - reserved_rows);
        let available_width_px = available_columns
            .checked_mul(i128::from(display.cell_width))
            .ok_or_else(overflow)?;
        let available_height_px = available_rows
            .checked_mul(i128::from(display.cell_height))
            .ok_or_else(overflow)?;
        let source_width = i128::from(source_width);
        let source_height = i128::from(source_height);
        let width_limited_height = available_width_px
            .checked_mul(source_height)
            .and_then(|value| value.checked_div(source_width))
            .ok_or_else(overflow)?;
        let (render_width_px, render_height_px) = if width_limited_height <= available_height_px {
            (available_width_px, width_limited_height)
        } else {
            let width = available_height_px
                .checked_mul(source_width)
                .and_then(|value| value.checked_div(source_height))
                .ok_or_else(overflow)?;
            (width, available_height_px)
        };
        let x_px = (available_width_px - render_width_px) / 2;
        let y_px = (available_height_px - render_height_px) / 2;
        let fixed = |pixels: i128, cell: u32| -> io::Result<i64> {
            pixels
                .checked_mul(FIXED_ONE)
                .and_then(|value| value.checked_div(i128::from(cell)))
                .and_then(|value| i64::try_from(value).ok())
                .ok_or_else(overflow)
        };
        Ok(Self {
            x: fixed(x_px, display.cell_width)?,
            y: fixed(y_px, display.cell_height)?,
            width: fixed(render_width_px, display.cell_width)?,
            height: fixed(render_height_px, display.cell_height)?,
            source_width: u32::try_from(source_width).expect("input was u32"),
            source_height: u32::try_from(source_height).expect("input was u32"),
            status_row: display.grid_rows - reserved_rows,
        })
    }

    pub fn node(self, node_id: u64, surface_context_id: u64, surface_id: u64) -> SceneNode {
        SceneNode {
            owning_context_id: surface_context_id,
            node_id,
            surface_context_id,
            surface_id,
            geometry: vec![
                (0, Value::Unsigned(COORDINATE_SPACE_GRID_CELL)),
                (1, Value::Unsigned(u64::try_from(self.x).unwrap_or(0))),
                (2, Value::Unsigned(u64::try_from(self.y).unwrap_or(0))),
                (3, Value::Unsigned(u64::try_from(self.width).unwrap_or(0))),
                (4, Value::Unsigned(u64::try_from(self.height).unwrap_or(0))),
                (5, Value::Unsigned(TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH)),
            ],
            fit: vivid_protocol::scene::Fit::Contain,
            linear_sampling: true,
            z_index: 0,
            visible: true,
            opacity: u16::MAX,
            clip: None,
        }
    }

    pub fn pointer(self, column: u16, row: u16) -> Option<(u32, u32)> {
        let cell_x = (i128::from(column) * FIXED_ONE) + FIXED_ONE / 2;
        let cell_y = (i128::from(row) * FIXED_ONE) + FIXED_ONE / 2;
        let x = cell_x.checked_sub(i128::from(self.x))?;
        let y = cell_y.checked_sub(i128::from(self.y))?;
        if x < 0 || y < 0 || x >= i128::from(self.width) || y >= i128::from(self.height) {
            return None;
        }
        let source_x = x
            .checked_mul(i128::from(self.source_width))?
            .checked_div(i128::from(self.width))?;
        let source_y = y
            .checked_mul(i128::from(self.source_height))?
            .checked_div(i128::from(self.height))?;
        Some((
            u32::try_from(source_x).ok()?.min(self.source_width - 1),
            u32::try_from(source_y).ok()?.min(self.source_height - 1),
        ))
    }

    pub fn pointer_clamped(self, column: u16, row: u16) -> Option<(u32, u32)> {
        if u32::from(row) >= self.status_row || self.width <= 0 || self.height <= 0 {
            return None;
        }
        let cell_x = (i128::from(column) * FIXED_ONE) + FIXED_ONE / 2;
        let cell_y = (i128::from(row) * FIXED_ONE) + FIXED_ONE / 2;
        let x = cell_x
            .saturating_sub(i128::from(self.x))
            .clamp(0, i128::from(self.width) - 1);
        let y = cell_y
            .saturating_sub(i128::from(self.y))
            .clamp(0, i128::from(self.height) - 1);
        Some((
            u32::try_from(x.saturating_mul(i128::from(self.source_width)) / i128::from(self.width))
                .ok()?
                .min(self.source_width - 1),
            u32::try_from(
                y.saturating_mul(i128::from(self.source_height)) / i128::from(self.height),
            )
            .ok()?
            .min(self.source_height - 1),
        ))
    }
}

fn overflow() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "scene geometry overflow")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(columns: u32, rows: u32) -> TerminalDisplay {
        TerminalDisplay {
            grid_columns: columns,
            grid_rows: rows,
            cell_width: 10,
            cell_height: 20,
        }
    }

    #[test]
    fn contains_wide_video_and_reserves_status_row() {
        let placement = Placement::calculate(display(80, 25), 1920, 1080).unwrap();
        assert!(placement.height <= 24_i64 << 32);
        assert!(placement.x >= 0);
        assert!(placement.y >= 0);
        assert!(placement.pointer(40, 12).is_some());
        assert!(placement.pointer(40, 24).is_none());
        assert!(placement.pointer_clamped(0, 10).is_some());
        assert!(placement.pointer_clamped(40, 24).is_none());
    }

    #[test]
    fn pointer_mapping_ignores_letterbox() {
        let placement = Placement::calculate(display(80, 25), 800, 1200).unwrap();
        assert!(placement.x > 0);
        assert!(placement.pointer(0, 10).is_none());
    }

    #[test]
    fn every_mapped_cell_lands_strictly_inside_the_source() {
        // The injectors reject an out-of-range absolute position rather than clamping it, so this
        // is the guarantee the terminal mouse path depends on: whatever cell the user points at,
        // the pixel handed to `TerminalInjector::pointer_absolute` is inside the capture. Both
        // letterboxed and pillarboxed geometries, and both mapping functions.
        for (source_width, source_height) in [(1920, 1080), (800, 1200), (640, 480), (65, 33)] {
            let placement =
                Placement::calculate(display(80, 25), source_width, source_height).unwrap();
            for row in 0..25_u16 {
                for column in 0..80_u16 {
                    for mapped in [
                        placement.pointer(column, row),
                        placement.pointer_clamped(column, row),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let (x, y) = mapped;
                        assert!(
                            x < source_width && y < source_height,
                            "cell ({column}, {row}) mapped to ({x}, {y}), outside \
                             {source_width}x{source_height}"
                        );
                    }
                }
            }
        }
    }
}
