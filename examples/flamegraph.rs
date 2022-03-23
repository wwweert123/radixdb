use std::{fs, time::Instant};

use log::info;
use radixdb::{DynBlobStore, PagedFileStore, TreeNode};
use tempfile::tempdir;

fn do_test(mut store: DynBlobStore) -> anyhow::Result<()> {
    let elems = (0..2000000).map(|i| {
        if i % 100000 == 0 {
            info!("{}", i);
        }
        (
            i.to_string().as_bytes().to_vec(),
            i.to_string().as_bytes().to_vec(),
        )
    });
    let t0 = Instant::now();
    info!("building tree");
    let mut tree: TreeNode = elems.collect();
    info!(
        "unattached tree {:?} {} s",
        tree,
        t0.elapsed().as_secs_f64()
    );
    info!("traversing unattached tree...");
    let t0 = Instant::now();
    let mut n = 0;
    for _ in tree.try_iter(&store)? {
        n += 1;
    }
    info!("done {} items, {} s", n, t0.elapsed().as_secs_f32());
    info!("attaching tree...");
    let t0 = Instant::now();
    tree.attach(&mut store)?;
    store.flush()?;
    info!("attached tree {:?} {} s", tree, t0.elapsed().as_secs_f32());
    info!("traversing attached tree values...");
    let t0 = Instant::now();
    let mut n = 0;
    for item in tree.try_values(&store) {
        if item.is_err() {
            info!("{:?}", item);
        }
        n += 1;
    }
    info!("done {} items, {} s", n, t0.elapsed().as_secs_f32());
    info!("traversing attached tree...");
    let t0 = Instant::now();
    let mut n = 0;
    for _ in tree.try_iter(&store)? {
        n += 1;
    }
    info!("done {} items, {} s", n, t0.elapsed().as_secs_f32());
    info!("detaching tree...");
    let t0 = Instant::now();
    tree.detach(&store, true)?;
    info!("detached tree {:?} {} s", tree, t0.elapsed().as_secs_f32());
    info!("traversing unattached tree...");
    let t0 = Instant::now();
    let mut n = 0;
    for _ in tree.try_iter(&store)? {
        n += 1;
    }
    info!("done {} items, {} s", n, t0.elapsed().as_secs_f32());
    Ok(())
}

fn init_logger() {
    let _ = env_logger::builder()
        // Include all events in tests
        .filter_level(log::LevelFilter::max())
        // Ensure events are captured by `cargo test`
        .is_test(true)
        // Ignore errors initializing the logger if tests race to configure it
        .try_init();
}

fn browser_compare() -> anyhow::Result<()> {
    init_logger();
    let dir = tempdir()?;
    let path = dir.path().join("large2.rdb");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let db = PagedFileStore::<1048576>::new(file).unwrap();
    let store: DynBlobStore = Box::new(db);
    do_test(store)
}

fn main() {
    browser_compare().unwrap()
}