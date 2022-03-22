use fnv::FnvHashMap;
use memmap::{Mmap, MmapMut, MmapOptions};
use parking_lot::Mutex;

use crate::{
    blob::{Blob, BlobOwner},
    blob_store::BlobStore,
    read_length_prefixed, write_length_prefixed, Hex,
};
use std::{
    fmt::Debug,
    fs::{self, File},
    io::{Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
};
use thousands::Separable;

#[derive(Debug, Clone)]
pub struct PagedFileStore<const SIZE: usize>(Arc<Mutex<Inner<SIZE>>>);

struct Inner<const SIZE: usize> {
    file: File,
    header: Header,   // header, for the size
    current: MmapMut, // current page
    pages: FnvHashMap<u64, Page<SIZE>>,
}

const ALIGN: usize = 8;

#[repr(C, align(8))]
struct PageInner<const SIZE: usize> {
    mmap: Mmap,
}

impl<const SIZE: usize> Debug for PageInner<SIZE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("PageInner<{}>", SIZE))
            .field("mmap", &&self.mmap)
            .finish()
    }
}

impl<const SIZE: usize> PageInner<SIZE> {
    fn new(mmap: Mmap) -> Self {
        assert!(mmap.len() == SIZE);
        Self { mmap }
    }
}

impl<const SIZE: usize> BlobOwner for Arc<PageInner<SIZE>> {
    fn get_slice(&self, offset: usize) -> &[u8] {
        read_length_prefixed(self.mmap.as_ref(), offset)
    }
}

#[derive(Debug, Clone)]
struct Page<const SIZE: usize>(Arc<dyn BlobOwner>);

impl<const SIZE: usize> Page<SIZE> {
    fn new(mmap: Mmap) -> Self {
        assert!(mmap.len() == SIZE);
        Self(Arc::new(Arc::new(PageInner::<SIZE>::new(mmap))))
    }

    /// try to get the bytes at the given offset
    fn bytes(&self, offset: usize) -> anyhow::Result<Blob<u8>> {
        anyhow::ensure!(offset + 4 < SIZE);
        Ok(Blob::<u8>::custom(self.0.clone(), offset))
    }
}

impl<const SIZE: usize> Debug for Inner<SIZE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PagedFileStore")
            .field("file", &self.file)
            .field("pages", &self.pages.len())
            .finish()
    }
}

fn pad_to(file: &mut File, offset: u64) -> anyhow::Result<()> {
    let padding = [0u8; 1024];
    loop {
        let pos = file.seek(std::io::SeekFrom::End(0))?;
        if pos < offset {
            let n = (offset - pos).min(1024);
            file.write(&padding[..n as usize])?;
        } else {
            break;
        }
    }
    Ok(())
}

const HEADER_SIZE: u64 = 1024;

struct Header {
    data: MmapMut,
}

impl Header {
    fn new(file: &mut File) -> anyhow::Result<Self> {
        // make room for the header
        pad_to(file, HEADER_SIZE)?;
        let data = unsafe {
            MmapOptions::new()
                .offset(0)
                .len(HEADER_SIZE as usize)
                .map_mut(&file)
        }?;
        Ok(Self { data })
    }

    fn size(&self) -> u64 {
        u64::from_be_bytes(self.data[0..8].try_into().unwrap())
    }

    fn set_size(&mut self, value: u64) -> anyhow::Result<()> {
        self.data[0..8].copy_from_slice(&u64::to_be_bytes(value));
        // Ok(self.data.flush()?)
        Ok(())
    }
}

impl<const PAGE_SIZE: usize> Inner<PAGE_SIZE> {
    pub fn new(mut file: File) -> anyhow::Result<Self> {
        assert!(PAGE_SIZE % ALIGN == 0);
        let header = Header::new(&mut file)?;
        let pages = pages(header.size(), PAGE_SIZE as u64).max(1);
        let size = pages * (PAGE_SIZE as u64) + HEADER_SIZE;
        // make sure the file is exactly as long as in the header.
        let file_size = file.seek(std::io::SeekFrom::End(0))?;
        if file_size > size {
            file.set_len(size)?;
        } else if file_size < size {
            pad_to(&mut file, size)?;
        }
        let current = Self::map_page_mut(&mut file, pages - 1)?;
        Ok(Self {
            file,
            header,
            current,
            pages: Default::default(),
        })
    }
    const fn page(offset: u64) -> u64 {
        offset / (PAGE_SIZE as u64)
    }
    // offset within a page
    const fn offset_within_page(offset: u64) -> usize {
        (offset % (PAGE_SIZE as u64)) as usize
    }
    // offset of a page, exluding the header
    const fn offset_of_page(page: u64) -> u64 {
        page * (PAGE_SIZE as u64)
    }
    fn current_page(&self) -> u64 {
        self.header.size() / (PAGE_SIZE as u64)
    }
    fn current_offset_in_page(&self) -> usize {
        (self.header.size() % (PAGE_SIZE as u64)) as usize
    }

    /// map page `page`. This will fail if the file does not extend over that page
    fn map_page(file: &File, page: u64) -> anyhow::Result<Mmap> {
        let page_start = page * (PAGE_SIZE as u64) + HEADER_SIZE;
        Ok(unsafe {
            MmapOptions::new()
                .offset(page_start)
                .len(PAGE_SIZE)
                .map(file)
        }?)
    }

    /// mutably map page `page`. This will extend the file to the required offset
    fn map_page_mut(file: &mut File, page: u64) -> anyhow::Result<MmapMut> {
        let page_start = page * (PAGE_SIZE as u64) + HEADER_SIZE;
        let page_end = page_start + (PAGE_SIZE as u64);
        pad_to(file, page_end)?;
        file.flush()?;
        Ok(unsafe {
            MmapOptions::new()
                .offset(page_start)
                .len(PAGE_SIZE)
                .map_mut(file)
        }?)
    }

    fn close_page(&mut self) -> anyhow::Result<()> {
        let current_page = self.current_page();
        let mut temp = Self::map_page_mut(&mut self.file, current_page + 1)?;
        std::mem::swap(&mut self.current, &mut temp);
        let current_page_data = temp.make_read_only()?;
        self.pages
            .insert(current_page, Page::new(current_page_data));
        self.header
            .set_size(Self::offset_of_page(current_page + 1))?;
        Ok(())
    }

    fn bytes(&mut self, offset: u64) -> anyhow::Result<Blob<u8>> {
        let page = Self::page(offset);
        let page_offset = Self::offset_within_page(offset);
        if let Some(page) = self.pages.get(&page) {
            page.bytes(page_offset as usize)
        } else if page <= self.current_page() {
            let mmap = Self::map_page(&self.file, page)?;
            let mmap = Page::new(mmap);
            let res = mmap.bytes(page_offset as usize);
            self.pages.insert(page, mmap);
            res
        } else {
            anyhow::bail!("page not found {}", page);
        }
    }

    fn append(&mut self, data: &[u8]) -> anyhow::Result<u64> {
        anyhow::ensure!(data.len() < PAGE_SIZE - 4, "block too large for this store");
        // len of the data when stored, including length prefix
        let len = data.len() as u64 + 4;
        let offset = self.header.size();
        // new end
        let end = offset + len;
        let current_page = Self::page(offset);
        let end_page = Self::page(end);
        // check if we cross a page boundary
        if end_page != current_page {
            self.close_page()?;
        }
        let offset = self.current_offset_in_page();
        // println!("{}.{}", current_page, offset);
        write_length_prefixed(self.current.as_mut(), offset, data);
        // self.current.flush()?;
        let offset = self.header.size();
        self.header.set_size(offset + len)?;
        Ok(offset)
    }
}

fn align(offset: u64) -> u64 {
    let mut res = offset;
    while (res % (ALIGN as u64)) != 0 {
        res += 1;
    }
    res
}

fn round_up_to(x: u64, size: u64) -> u64 {
    let q = x / size;
    let r = x % size;
    if r == 0 {
        x
    } else {
        (q + 1) * size
    }
}

fn pages(size: u64, page_size: u64) -> u64 {
    let q = size / page_size;
    let r = size % page_size;
    if r == 0 {
        q
    } else {
        q + 1
    }
}

impl<const SIZE: usize> PagedFileStore<SIZE> {
    pub fn new(file: File) -> anyhow::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(Inner::<SIZE>::new(file)?))))
    }
}

impl<const SIZE: usize> BlobStore for PagedFileStore<SIZE> {
    fn bytes(&self, id: u64) -> anyhow::Result<Blob<u8>> {
        self.0.lock().bytes(id)
    }

    fn append(&self, data: &[u8]) -> anyhow::Result<u64> {
        self.0.lock().append(data)
    }

    fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use proptest::prelude::*;
    use tempfile::tempdir;

    const TEST_SIZE: usize = 1024;

    fn large_blocks() -> impl Strategy<Value = Vec<Vec<u8>>> {
        proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..TEST_SIZE - 4),
            1..10,
        )
    }

    fn small_blocks() -> impl Strategy<Value = Vec<Vec<u8>>> {
        proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..TEST_SIZE / 10),
            1..100,
        )
    }

    fn test_blocks() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop_oneof![large_blocks(), small_blocks(),]
    }

    fn ok<T>(f: impl Fn() -> anyhow::Result<T>) -> T {
        let res = f();
        res.ok().unwrap()
    }

    fn mk_block<const S: usize>(i: u64) -> [u8; S] {
        let mut data = [0u8; S];
        data[0..8].copy_from_slice(&i.to_be_bytes());
        data
    }

    #[test]
    // #[ignore = "too large"]
    fn paged_file_store_test_large() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("large.rdb");
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        println!("writing all of {:?}", path);
        let t = Instant::now();
        let db = PagedFileStore::<1048576>::new(file).unwrap();
        const BLOCK_SIZE: usize = 6666;
        const BLOCK_COUNT: u64 = 1000000;
        const TOTAL_SIZE: u64 = (BLOCK_SIZE as u64) * BLOCK_COUNT;
        let offsets = (0u64..BLOCK_COUNT)
            .map(|i| {
                let data = mk_block::<BLOCK_SIZE>(i);
                db.append(&data)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let dt = t.elapsed().as_secs_f64();
        println!(
            "done with {:?}, {}s, {}b/s",
            path,
            dt,
            ((TOTAL_SIZE as f64) / dt)
                .floor()
                .separate_with_underscores()
        );
        println!("reading all of {:?}", path);
        let t = Instant::now();
        for (i, offset) in offsets.into_iter().enumerate() {
            let expected = mk_block::<BLOCK_SIZE>(i as u64);
            let actual = db.bytes(offset)?;
            assert_eq!(&expected[..], actual.as_ref());
        }
        let dt = t.elapsed().as_secs_f64();
        println!(
            "done with {:?}, {}s, {}b/s",
            path,
            dt,
            ((TOTAL_SIZE as f64) / dt)
                .floor()
                .separate_with_underscores()
        );
        println!("total size {}", (BLOCK_SIZE as u64) * BLOCK_COUNT);
        Ok(())
    }

    proptest! {

        #[test]
        fn paged_file_store_test(blocks in test_blocks()) {
            let file = tempfile::tempfile().unwrap();
            let mut store = Inner::<TEST_SIZE>::new(file).unwrap();
            let res =
                blocks
                    .into_iter()
                    .map(|block| store.append(block.as_ref())
                        .map(|offset| (offset, block))).collect::<anyhow::Result<Vec<_>>>().unwrap();
            for (offset, block) in res.iter() {
                let actual = store.bytes(*offset).unwrap();
                let expected: &[u8] = &block;
                prop_assert_eq!(actual.as_ref(), expected);
            }
            println!("{:?}", store);
            for page in store.pages.values() {
                println!("{:?}", page);
            }
            println!();
        }
    }
}