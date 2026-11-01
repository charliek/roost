//! A terminal's Kitty image placements in `tab.dump_resolved`'s wire
//! types — shared by the session tab task and roost-iced's terminals.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use roost_ipc::messages::{ResolvedImage, ResolvedImageLayer, ResolvedImageSource};
use roost_vt::{ImageLayer, ImagePlacement, Terminal};

/// Each placed image's pixel size, keyed by image id; 0 × 0 while an
/// image's pixels have not arrived.
pub fn sizes(
    terminal: &Terminal,
    placements: &[ImagePlacement],
) -> roost_vt::Result<HashMap<u32, (u32, u32)>> {
    let mut sizes = HashMap::new();
    for placement in placements {
        if let Entry::Vacant(slot) = sizes.entry(placement.image_id) {
            let size = terminal
                .kitty_image(placement.image_id)?
                .map_or((0, 0), |image| (image.width(), image.height()));
            slot.insert(size);
        }
    }
    Ok(sizes)
}

/// `placements` as the wire lists them, in the same order.
pub fn resolved(
    placements: &[ImagePlacement],
    sizes: &HashMap<u32, (u32, u32)>,
) -> Vec<ResolvedImage> {
    placements
        .iter()
        .map(|placement| {
            let (image_width, image_height) =
                sizes.get(&placement.image_id).copied().unwrap_or_default();
            to_wire(placement, image_width, image_height)
        })
        .collect()
}

fn to_wire(placement: &ImagePlacement, image_width: u32, image_height: u32) -> ResolvedImage {
    ResolvedImage {
        image_id: placement.image_id,
        placement_id: placement.placement_id,
        layer: match placement.layer {
            ImageLayer::BelowBackground => ResolvedImageLayer::BelowBackground,
            ImageLayer::BelowText => ResolvedImageLayer::BelowText,
            ImageLayer::AboveText => ResolvedImageLayer::AboveText,
        },
        z: placement.z,
        col: placement.col,
        row: placement.row,
        grid_cols: placement.grid_cols,
        grid_rows: placement.grid_rows,
        offset_x: placement.offset_x,
        offset_y: placement.offset_y,
        pixel_width: placement.pixel_width,
        pixel_height: placement.pixel_height,
        source: ResolvedImageSource {
            x: placement.source.x,
            y: placement.source.y,
            width: placement.source.width,
            height: placement.source.height,
        },
        image_width,
        image_height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_vt::{PixelRect, PlacementKind};

    #[test]
    fn every_placement_field_lands_on_its_own_wire_field() {
        let placement = ImagePlacement {
            image_id: 1,
            generation: 99,
            layer: ImageLayer::AboveText,
            z: -3,
            col: -4,
            row: -5,
            grid_cols: 6,
            grid_rows: 7,
            offset_x: 8,
            offset_y: 9,
            pixel_width: 10,
            pixel_height: 11,
            source: PixelRect {
                x: 12,
                y: 13,
                width: 14,
                height: 15,
            },
            placement_id: 16,
            kind: PlacementKind::Fixed,
        };
        let below = ImagePlacement {
            image_id: 2,
            layer: ImageLayer::BelowBackground,
            ..placement
        };
        let text = ImagePlacement {
            image_id: 3,
            layer: ImageLayer::BelowText,
            ..placement
        };
        let sizes = HashMap::from([(1, (17, 18)), (2, (19, 20))]);

        let wire = resolved(&[placement, below, text], &sizes);

        assert_eq!(
            wire[0],
            ResolvedImage {
                image_id: 1,
                placement_id: 16,
                layer: ResolvedImageLayer::AboveText,
                z: -3,
                col: -4,
                row: -5,
                grid_cols: 6,
                grid_rows: 7,
                offset_x: 8,
                offset_y: 9,
                pixel_width: 10,
                pixel_height: 11,
                source: ResolvedImageSource {
                    x: 12,
                    y: 13,
                    width: 14,
                    height: 15,
                },
                image_width: 17,
                image_height: 18,
            }
        );
        assert_eq!(
            (wire[1].layer, wire[1].image_width, wire[1].image_height),
            (ResolvedImageLayer::BelowBackground, 19, 20)
        );
        assert_eq!(
            (wire[2].layer, wire[2].image_width, wire[2].image_height),
            (ResolvedImageLayer::BelowText, 0, 0),
            "an image with no recorded size is listed as 0 x 0"
        );
    }
}
