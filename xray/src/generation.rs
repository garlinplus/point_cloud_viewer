// Code related to X-Ray generation.

use crate::colormap::{Colormap, Jet, Monochrome, PURPLISH};
use crate::utils::{get_image_path, get_meta_pb_path};
use crate::Meta;
use clap::arg_enum;
use fnv::{FnvHashMap, FnvHashSet};
use image::{self, GenericImage, ImageResult, Rgba, RgbaImage};
use imageproc::map::map_colors;
use nalgebra::{Isometry3, Point2, Point3, Vector2};
use num::clamp;
use point_cloud_client::PointCloudClient;
use point_viewer::attributes::AttributeData;
use point_viewer::color::{Color, TRANSPARENT, WHITE};
use point_viewer::geometry::{Aabb, Obb};
use point_viewer::iterator::{PointLocation, PointQuery};
use point_viewer::math::ClosedInterval;
use point_viewer::utils::create_syncable_progress_bar;
use point_viewer::{match_1d_attr_data, PointsBatch};
use quadtree::{ChildIndex, Node, NodeId, Rect};
use rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator};
use stats::OnlineStats;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

// The number of Z-buckets we subdivide our bounding cube into along the z-direction. This affects
// the saturation of a point in x-rays: the more buckets contain a point, the darker the pixel
// becomes.
const NUM_Z_BUCKETS: f64 = 1024.;

arg_enum! {
    #[derive(Debug)]
    #[allow(non_camel_case_types)]
    pub enum ColoringStrategyArgument {
        xray,
        colored,
        colored_with_intensity,
        colored_with_height_stddev,
    }
}

arg_enum! {
    #[derive(Debug)]
    #[allow(non_camel_case_types)]
    pub enum TileBackgroundColorArgument {
        white,
        transparent,
    }
}

impl TileBackgroundColorArgument {
    pub fn to_color(&self) -> Color<u8> {
        match self {
            TileBackgroundColorArgument::white => WHITE.to_u8(),
            TileBackgroundColorArgument::transparent => TRANSPARENT.to_u8(),
        }
    }
}

arg_enum! {
    #[derive(Debug)]
    #[allow(non_camel_case_types)]
    pub enum ColormapArgument {
        jet,
        purplish,
    }
}

// Maps from attribute name to the bin size
type Binning = Option<(String, f64)>;

#[derive(Debug)]
pub enum ColoringStrategyKind {
    XRay,
    Colored(Binning),

    // Min and max intensities.
    ColoredWithIntensity(f32, f32, Binning),

    // Colored in heat-map colors by stddev. Takes the max stddev to clamp on.
    ColoredWithHeightStddev(f32, ColormapArgument),
}

impl ColoringStrategyKind {
    pub fn new_strategy(&self) -> Box<dyn ColoringStrategy> {
        use ColoringStrategyKind::*;
        match self {
            XRay => Box::new(XRayColoringStrategy::new()),
            Colored(binning) => Box::new(PointColorColoringStrategy::new(binning.clone())),
            ColoredWithIntensity(min_intensity, max_intensity, binning) => Box::new(
                IntensityColoringStrategy::new(*min_intensity, *max_intensity, binning.clone()),
            ),
            ColoredWithHeightStddev(max_stddev, ColormapArgument::jet) => {
                Box::new(HeightStddevColoringStrategy::new(*max_stddev, Jet {}))
            }
            ColoredWithHeightStddev(max_stddev, ColormapArgument::purplish) => Box::new(
                HeightStddevColoringStrategy::new(*max_stddev, Monochrome(PURPLISH)),
            ),
        }
    }
}

pub trait ColoringStrategy: Send {
    // Processes points that have been discretized into the pixels (x, y) and the z columns according
    // to NUM_Z_BUCKETS.
    fn process_discretized_point_data(
        &mut self,
        points_batch: &PointsBatch,
        discretized_locations: Vec<Point3<u32>>,
    );

    fn process_point_data(
        &mut self,
        points_batch: &PointsBatch,
        bbox: &Aabb<f64>,
        image_size: Vector2<u32>,
    ) {
        let mut discretized_locations = Vec::with_capacity(points_batch.position.len());
        for pos in &points_batch.position {
            // We want a right handed coordinate system with the x-axis of world and images aligning.
            // This means that the y-axis aligns too, but the origin of the image space must be at the
            // bottom left. Since images have their origin at the top left, we need actually have to
            // invert y and go from the bottom of the image.
            let x = (((pos.x - bbox.min().x) / bbox.diag().x) * f64::from(image_size.x)) as u32;
            let y =
                ((1. - ((pos.y - bbox.min().y) / bbox.diag().y)) * f64::from(image_size.y)) as u32;
            let z = (((pos.z - bbox.min().z) / bbox.diag().z) * NUM_Z_BUCKETS) as u32;
            discretized_locations.push(Point3::new(x, y, z));
        }
        self.process_discretized_point_data(points_batch, discretized_locations)
    }

    // After all points are processed, this is used to query the color that should be assigned to
    // the pixel (x, y) in the final tile image.
    fn get_pixel_color(&self, x: u32, y: u32) -> Option<Color<u8>>;

    fn attributes(&self) -> HashSet<String> {
        HashSet::default()
    }
}

trait BinnedColoringStrategy {
    fn binning(&self) -> &Binning;
    fn bins(&self, points_batch: &PointsBatch) -> Vec<i64> {
        match self.binning() {
            Some((attrib_name, size)) => {
                let attr_data = points_batch
                    .attributes
                    .get(attrib_name)
                    .expect("Binning attribute needs to be available in points batch.");
                macro_rules! rhs {
                    ($dtype:ident, $data:ident, $size:expr) => {
                        $data.iter().map(|e| (*e as f64 / *$size) as i64).collect()
                    };
                }
                match_1d_attr_data!(attr_data, rhs, size)
            }
            None => vec![0; points_batch.position.len()],
        }
    }
}

struct XRayColoringStrategy {
    z_buckets: FnvHashMap<(u32, u32), FnvHashSet<u32>>,
    max_saturation: f64,
}

impl XRayColoringStrategy {
    fn new() -> Self {
        XRayColoringStrategy {
            z_buckets: FnvHashMap::default(),
            // TODO(sirver): Once 'const fn' lands, this constant can be calculated at compile time.
            max_saturation: NUM_Z_BUCKETS.ln(),
        }
    }
}

impl ColoringStrategy for XRayColoringStrategy {
    fn process_discretized_point_data(
        &mut self,
        _: &PointsBatch,
        discretized_locations: Vec<Point3<u32>>,
    ) {
        for d_loc in discretized_locations {
            let z_buckets = self.z_buckets.entry((d_loc.x, d_loc.y)).or_default();
            z_buckets.insert(d_loc.z);
        }
    }

    fn get_pixel_color(&self, x: u32, y: u32) -> Option<Color<u8>> {
        self.z_buckets.get(&(x, y)).map(|z| {
            let saturation = (z.len() as f64).ln() / self.max_saturation;
            let value = ((1. - saturation) * 255.) as u8;
            Color {
                red: value,
                green: value,
                blue: value,
                alpha: 255,
            }
        })
    }
}

#[derive(Default)]
struct PerColumnData<T> {
    // The sum of all seen values.
    sum: T,
    // The number of all points that landed in this column.
    count: usize,
}

type IntensityPerColumnData = FnvHashMap<(u32, u32), FnvHashMap<i64, PerColumnData<f32>>>;

struct IntensityColoringStrategy {
    min: f32,
    max: f32,
    binning: Binning,
    per_column_data: IntensityPerColumnData,
}

impl IntensityColoringStrategy {
    fn new(min: f32, max: f32, binning: Binning) -> Self {
        IntensityColoringStrategy {
            min,
            max,
            binning,
            per_column_data: FnvHashMap::default(),
        }
    }
}

impl BinnedColoringStrategy for IntensityColoringStrategy {
    fn binning(&self) -> &Binning {
        &self.binning
    }
}

impl ColoringStrategy for IntensityColoringStrategy {
    fn process_discretized_point_data(
        &mut self,
        points_batch: &PointsBatch,
        discretized_locations: Vec<Point3<u32>>,
    ) {
        let bins = self.bins(points_batch);
        let intensity_attribute = points_batch
            .attributes
            .get("intensity")
            .expect("Coloring by intensity was requested, but point data without intensity found.");
        if let AttributeData::F32(intensity_vec) = intensity_attribute {
            for i in 0..intensity_vec.len() {
                let intensity = intensity_vec[i];
                if intensity < 0. {
                    return;
                }
                let per_column_data = self
                    .per_column_data
                    .entry((discretized_locations[i].x, discretized_locations[i].y))
                    .or_default();
                let bin_data = per_column_data.entry(bins[i]).or_default();
                bin_data.sum += intensity;
                bin_data.count += 1;
            }
        }
    }

    fn get_pixel_color(&self, x: u32, y: u32) -> Option<Color<u8>> {
        self.per_column_data.get(&(x, y)).map(|c| {
            let mean = (c
                .values()
                .map(|bin_data| bin_data.sum / bin_data.count as f32)
                .sum::<f32>()
                / c.len() as f32)
                .max(self.min)
                .min(self.max);
            let brighten = (mean - self.min).ln() / (self.max - self.min).ln();
            Color {
                red: brighten,
                green: brighten,
                blue: brighten,
                alpha: 1.,
            }
            .to_u8()
        })
    }

    fn attributes(&self) -> HashSet<String> {
        let mut attributes = HashSet::default();
        attributes.insert("intensity".into());
        if let Some((attr_name, _)) = &self.binning {
            attributes.insert(attr_name.clone());
        }
        attributes
    }
}

type PointColorPerColumnData = FnvHashMap<(u32, u32), FnvHashMap<i64, PerColumnData<Color<f32>>>>;

struct PointColorColoringStrategy {
    binning: Binning,
    per_column_data: PointColorPerColumnData,
}

impl PointColorColoringStrategy {
    fn new(binning: Binning) -> Self {
        Self {
            binning,
            per_column_data: FnvHashMap::default(),
        }
    }
}

impl BinnedColoringStrategy for PointColorColoringStrategy {
    fn binning(&self) -> &Binning {
        &self.binning
    }
}

impl ColoringStrategy for PointColorColoringStrategy {
    fn process_discretized_point_data(
        &mut self,
        points_batch: &PointsBatch,
        discretized_locations: Vec<Point3<u32>>,
    ) {
        let bins = self.bins(points_batch);
        let color_attribute = points_batch
            .attributes
            .get("color")
            .expect("Coloring was requested, but point data without color found.");
        if let AttributeData::U8Vec3(color_vec) = color_attribute {
            for i in 0..color_vec.len() {
                let color = Color::<u8> {
                    red: color_vec[i][0],
                    green: color_vec[i][1],
                    blue: color_vec[i][2],
                    alpha: 255,
                }
                .to_f32();
                let per_column_data = self
                    .per_column_data
                    .entry((discretized_locations[i].x, discretized_locations[i].y))
                    .or_default();
                let bin_data = per_column_data.entry(bins[i]).or_default();
                bin_data.sum += color;
                bin_data.count += 1;
            }
        }
    }

    fn get_pixel_color(&self, x: u32, y: u32) -> Option<Color<u8>> {
        self.per_column_data.get(&(x, y)).map(|c| {
            (c.values()
                .map(|bin_data| bin_data.sum / bin_data.count as f32)
                .sum::<Color<f32>>()
                / c.len() as f32)
                .to_u8()
        })
    }

    fn attributes(&self) -> HashSet<String> {
        let mut attributes = HashSet::default();
        attributes.insert("color".into());
        if let Some((attr_name, _)) = &self.binning {
            attributes.insert(attr_name.clone());
        }
        attributes
    }
}

struct HeightStddevColoringStrategy<C: Colormap> {
    per_column_data: FnvHashMap<(u32, u32), OnlineStats>,
    max_stddev: f32,
    colormap: C,
}

impl<C: Colormap> HeightStddevColoringStrategy<C> {
    fn new(max_stddev: f32, colormap: C) -> Self {
        HeightStddevColoringStrategy {
            max_stddev,
            per_column_data: FnvHashMap::default(),
            colormap,
        }
    }
}

impl<C: Colormap> ColoringStrategy for HeightStddevColoringStrategy<C> {
    fn process_discretized_point_data(
        &mut self,
        points_batch: &PointsBatch,
        discretized_locations: Vec<Point3<u32>>,
    ) {
        for (i, d_loc) in discretized_locations
            .iter()
            .enumerate()
            .take(discretized_locations.len())
        {
            self.per_column_data
                .entry((d_loc.x, d_loc.y))
                .or_insert_with(OnlineStats::new)
                .add(points_batch.position[i].z);
        }
    }

    fn get_pixel_color(&self, x: u32, y: u32) -> Option<Color<u8>> {
        self.per_column_data.get(&(x, y)).map(|c| {
            let saturation = clamp(c.stddev() as f32, 0., self.max_stddev) / self.max_stddev;
            self.colormap.for_value(saturation)
        })
    }
}

/// Build a parent image created of the 4 children tiles. All tiles are optionally, in which case
/// they are left white in the resulting image. The input images must be square with length N,
/// the returned image is square with length 2*N.
pub fn build_parent(children: &[Option<RgbaImage>], tile_background_color: Color<u8>) -> RgbaImage {
    assert_eq!(children.len(), 4);
    let mut child_size_px = None;
    for c in children.iter() {
        if c.is_none() {
            continue;
        }
        let c = c.as_ref().unwrap();
        assert_eq!(
            c.width(),
            c.height(),
            "Expected width to be equal to height."
        );
        match child_size_px {
            None => child_size_px = Some(c.width()),
            Some(w) => {
                assert_eq!(w, c.width(), "Not all images have the same size.");
            }
        }
    }
    let child_size_px = child_size_px.expect("No children passed to 'build_parent'.");
    let mut large_image = RgbaImage::from_pixel(
        child_size_px * 2,
        child_size_px * 2,
        Rgba::from(tile_background_color),
    );

    // We want the x-direction to be up in the octree. Since (0, 0) is the top left
    // position in the image, we actually have to invert y and go from the bottom
    // of the image.
    for &(id, xoffs, yoffs) in &[
        (1, 0, 0),
        (0, 0, child_size_px),
        (3, child_size_px, 0),
        (2, child_size_px, child_size_px),
    ] {
        if let Some(ref img) = children[id] {
            large_image.copy_from(img, xoffs, yoffs).unwrap();
        }
    }
    large_image
}

pub struct XrayParameters {
    pub output_directory: PathBuf,
    pub point_cloud_client: PointCloudClient,
    pub query_from_global: Option<Isometry3<f64>>,
    pub filter_intervals: HashMap<String, ClosedInterval<f64>>,
    pub tile_background_color: Color<u8>,
    pub tile_size_px: u32,
    pub pixel_size_m: f64,
    pub root_node_id: NodeId,
}

pub fn xray_from_points(
    bbox: &Aabb<f64>,
    image_size: Vector2<u32>,
    mut coloring_strategy: Box<dyn ColoringStrategy>,
    parameters: &XrayParameters,
) -> Option<RgbaImage> {
    let mut seen_any_points = false;
    let location = match &parameters.query_from_global {
        Some(query_from_global) => {
            let global_from_query = query_from_global.inverse();
            PointLocation::Obb(Obb::from(bbox).transformed(&global_from_query))
        }
        None => PointLocation::Aabb(bbox.clone()),
    };
    let mut attributes = coloring_strategy.attributes();
    attributes.extend(parameters.filter_intervals.keys().cloned());
    let point_query = PointQuery {
        attributes: attributes.iter().map(|a| a.as_ref()).collect(),
        location,
        filter_intervals: parameters
            .filter_intervals
            .iter()
            .map(|(k, v)| (&k[..], *v))
            .collect(),
    };
    let _ = parameters
        .point_cloud_client
        .for_each_point_data(&point_query, |mut points_batch| {
            seen_any_points = true;
            if let Some(query_from_global) = &parameters.query_from_global {
                for p in &mut points_batch.position {
                    *p = query_from_global.transform_point(p);
                }
            }
            coloring_strategy.process_point_data(&points_batch, bbox, image_size);
            Ok(())
        });

    if !seen_any_points {
        return None;
    }

    let mut image = RgbaImage::new(image_size.x, image_size.y);
    let background_color = Rgba::from(TRANSPARENT.to_u8());
    for (x, y, i) in image.enumerate_pixels_mut() {
        let pixel_color = coloring_strategy.get_pixel_color(x, y);
        *i = pixel_color.map(Rgba::from).unwrap_or(background_color);
    }
    Some(image)
}

pub fn find_quadtree_bounding_rect_and_levels(
    bbox: &Aabb<f64>,
    tile_size_px: u32,
    pixel_size_m: f64,
) -> (Rect, u8) {
    let tile_size_m = f64::from(tile_size_px) * pixel_size_m;
    let mut levels = 0;
    let mut cur_size = tile_size_m;
    let diag = bbox.diag();
    while cur_size < diag.x || cur_size < diag.y {
        cur_size *= 2.;
        levels += 1;
    }
    (
        Rect::new(Point2::new(bbox.min().x, bbox.min().y), cur_size),
        levels,
    )
}

pub fn get_nodes_at_level(root_node: &Node, level: u8) -> Vec<Node> {
    let mut nodes_at_level = Vec::with_capacity(4usize.pow((level - root_node.level()).into()));
    let mut nodes_to_traverse = Vec::with_capacity((4 * nodes_at_level.capacity() - 1) / 3);
    nodes_to_traverse.push(root_node.clone());
    while let Some(node) = nodes_to_traverse.pop() {
        if node.level() == level {
            nodes_at_level.push(node);
        } else {
            for i in 0..4 {
                nodes_to_traverse.push(node.get_child(&ChildIndex::from_u8(i)));
            }
        }
    }
    nodes_at_level
}

pub fn get_bounding_box(
    bounding_box: &Aabb<f64>,
    query_from_global: &Option<Isometry3<f64>>,
) -> Aabb<f64> {
    match query_from_global {
        Some(query_from_global) => bounding_box.transform(&query_from_global),
        None => bounding_box.clone(),
    }
}

pub fn build_xray_quadtree(
    coloring_strategy_kind: &ColoringStrategyKind,
    parameters: &XrayParameters,
) -> Result<(), Box<dyn Error>> {
    // Ignore errors, maybe directory is already there.
    let _ = fs::create_dir(&parameters.output_directory);

    let bounding_box = get_bounding_box(
        &parameters.point_cloud_client.bounding_box(),
        &parameters.query_from_global,
    );
    let (bounding_rect, deepest_level) = find_quadtree_bounding_rect_and_levels(
        &bounding_box,
        parameters.tile_size_px,
        parameters.pixel_size_m,
    );

    let root_node_id = parameters.root_node_id;
    let root_level = root_node_id.level();
    assert!(
        root_level <= deepest_level,
        "Specified root node id is outside quadtree."
    );
    let root_node = Node::from_node_id_and_root_bounding_rect(root_node_id, bounding_rect);
    let leaf_nodes = get_nodes_at_level(&root_node, deepest_level);

    let created_leaf_node_ids = create_leaf_nodes(
        leaf_nodes,
        deepest_level,
        &bounding_box,
        coloring_strategy_kind,
        parameters,
    )?;

    assign_background_color(
        &parameters.output_directory,
        parameters.tile_background_color,
        &created_leaf_node_ids,
    )?;

    let all_node_ids = create_non_leaf_nodes(
        created_leaf_node_ids,
        deepest_level,
        root_level,
        &parameters.output_directory,
        parameters.tile_background_color,
        parameters.tile_size_px,
    );

    let meta = Meta {
        nodes: all_node_ids,
        bounding_rect: root_node.bounding_rect,
        tile_size: parameters.tile_size_px,
        deepest_level,
    };
    meta.to_disk(get_meta_pb_path(&parameters.output_directory, root_node_id))
        .expect("Filed to write meta file to disk.");

    Ok(())
}

pub fn create_leaf_nodes(
    leaf_nodes: Vec<Node>,
    deepest_level: u8,
    bounding_box: &Aabb<f64>,
    coloring_strategy_kind: &ColoringStrategyKind,
    parameters: &XrayParameters,
) -> ImageResult<FnvHashSet<NodeId>> {
    let (created_leaf_node_ids_tx, created_leaf_node_ids_rx) = crossbeam::channel::unbounded();
    let progress_bar = create_syncable_progress_bar(
        leaf_nodes.len(),
        &format!("Building level {}", deepest_level),
    );
    leaf_nodes
        .into_par_iter()
        .try_for_each(|node| -> ImageResult<()> {
            let strategy: Box<dyn ColoringStrategy> = coloring_strategy_kind.new_strategy();
            let rect_min = node.bounding_rect.min();
            let rect_max = node.bounding_rect.max();
            let min = Point3::new(rect_min.x, rect_min.y, bounding_box.min().z);
            let max = Point3::new(rect_max.x, rect_max.y, bounding_box.max().z);
            let bbox = Aabb::new(min, max);
            if let Some(image) = xray_from_points(
                &bbox,
                Vector2::new(parameters.tile_size_px, parameters.tile_size_px),
                strategy,
                parameters,
            ) {
                image.save(&get_image_path(&parameters.output_directory, node.id))?;
                created_leaf_node_ids_tx.send(node.id).unwrap();
            }
            progress_bar.lock().unwrap().inc();
            Ok(())
        })?;
    progress_bar.lock().unwrap().finish_println("");
    drop(created_leaf_node_ids_tx);
    Ok(created_leaf_node_ids_rx.into_iter().collect())
}

pub fn create_non_leaf_nodes(
    created_leaf_node_ids: FnvHashSet<NodeId>,
    deepest_level: u8,
    root_level: u8,
    output_directory: &Path,
    tile_background_color: Color<u8>,
    tile_size_px: u32,
) -> FnvHashSet<NodeId> {
    let mut current_level_nodes = created_leaf_node_ids;
    let mut all_nodes = current_level_nodes.clone();

    for current_level in (root_level..deepest_level).rev() {
        current_level_nodes = current_level_nodes
            .iter()
            .filter_map(|node| node.parent_id())
            .collect();
        build_level(
            output_directory,
            tile_size_px,
            current_level,
            &current_level_nodes,
            tile_background_color,
        );
        all_nodes.extend(&current_level_nodes);
    }
    all_nodes
}

pub fn assign_background_color(
    output_directory: &Path,
    tile_background_color: Color<u8>,
    created_leaf_node_ids: &FnvHashSet<NodeId>,
) -> ImageResult<()> {
    let progress_bar =
        create_syncable_progress_bar(created_leaf_node_ids.len(), "Assigning background color");
    let background_color = Rgba::from(tile_background_color);
    created_leaf_node_ids
        .par_iter()
        .try_for_each(|node_id| -> ImageResult<()> {
            let image_path = get_image_path(output_directory, *node_id);
            let mut image = image::open(&image_path)?.to_rgba();
            // Depending on the implementation of the inpainting function above we may get pixels
            // that are not fully opaque or fully transparent. This is why we choose a threshold
            // in the middle to consider pixels as background or foreground and could be reevaluated
            // in the future.
            image = map_colors(&image, |p| if p[3] < 128 { background_color } else { p });
            image.save(&image_path)?;
            progress_bar.lock().unwrap().inc();
            Ok(())
        })?;
    progress_bar.lock().unwrap().finish_println("");
    Ok(())
}

pub fn build_level(
    output_directory: &Path,
    tile_size_px: u32,
    current_level: u8,
    nodes: &FnvHashSet<NodeId>,
    tile_background_color: Color<u8>,
) {
    let progress_bar =
        create_syncable_progress_bar(nodes.len(), &format!("Building level {}", current_level));
    nodes.par_iter().for_each(|node| {
        build_node(output_directory, *node, tile_size_px, tile_background_color);
        progress_bar.lock().unwrap().inc();
    });
    progress_bar.lock().unwrap().finish_println("");
}

fn build_node(
    output_directory: &Path,
    node_id: NodeId,
    tile_size_px: u32,
    tile_background_color: Color<u8>,
) {
    let mut children = [None, None, None, None];
    // We a right handed coordinate system with the x-axis of world and images
    // aligning. This means that the y-axis aligns too, but the origin of the image
    // space must be at the bottom left. Since images have their origin at the top
    // left, we need actually have to invert y and go from the bottom of the image.
    for id in 0..4 {
        let png = get_image_path(
            output_directory,
            node_id.get_child_id(&ChildIndex::from_u8(id)),
        );
        if png.exists() {
            children[id as usize] = Some(image::open(&png).unwrap().to_rgba());
        }
    }
    if children.iter().any(|child| child.is_some()) {
        let large_image = build_parent(&children, tile_background_color);
        let image = image::DynamicImage::ImageRgba8(large_image).resize(
            tile_size_px,
            tile_size_px,
            image::imageops::FilterType::Lanczos3,
        );
        image
            .as_rgba8()
            .unwrap()
            .save(&get_image_path(output_directory, node_id))
            .unwrap();
    }
}
