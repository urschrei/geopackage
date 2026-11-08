//! Where the fixed cost of a small read goes: run with a file, a layer and a
//! bounding box, e.g. `read_cost big.gpkg pts 40 40 50 50`.
use std::time::Instant;

use geopackage::arrow::ArrowReadOptions;
use geopackage::{BoundingBox, GeoPackage};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, path, layer, a, b, c, d] = args.as_slice() else {
        return Err("usage: read_cost FILE LAYER MINX MINY MAXX MAXY".into());
    };
    let bbox = BoundingBox::new(a.parse()?, b.parse()?, c.parse()?, d.parse()?);
    for round in 0..3 {
        let t = Instant::now();
        let gpkg = GeoPackage::open_read_only_lenient(path)?;
        let opened = t.elapsed();
        let l = gpkg.layer(layer)?;
        let resolved = t.elapsed();
        let mut rows = 0;
        let mut first = None;
        for batch in l.read_arrow_in(bbox, ArrowReadOptions::default())? {
            rows += batch?.num_rows();
            first.get_or_insert(t.elapsed());
        }
        let done = t.elapsed();
        let t2 = Instant::now();
        let n = l.features_in(bbox)?.count();
        let row_path = t2.elapsed();
        println!(
            "round {round}: open {opened:?}, layer {:?}, first batch {:?}, all {rows} rows {done:?}; row path {n} rows {row_path:?}",
            resolved - opened,
            first.map(|f| f - resolved)
        );
    }
    Ok(())
}
