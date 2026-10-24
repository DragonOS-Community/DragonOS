//! Canonical-map boundaries on a caller-owned real physical backing. No live
//! buddy or global placement metadata is changed by these private map owners.
use super::*;

fn area(base: usize, size: usize) -> PhysMemoryArea {
    PhysMemoryArea::new(PhysAddr::new(base), size, MemoryAreaAttr::empty())
}

unsafe fn initialize(storage: PhysAddr, areas: &[PhysMemoryArea]) -> Option<PlacementMap> {
    let (spans, blocks, _) = PlacementMap::requirements(areas)?;
    PlacementMap::initialize::<MMArch>(storage, MMArch::PAGE_SIZE, spans, blocks, areas)
}

unsafe fn checks(metadata: PhysAddr, arena: PhysAddr) -> Option<bool> {
    let page = MMArch::PAGE_SIZE;
    let block = 1usize << BLOCK_ORDER;
    // The enclosing report owns this metadata prefix and a disjoint aligned
    // arena. The map tests use three pages of the prefix and three pageblocks
    // of physical identities, without accessing those arena bytes.
    if !metadata.check_aligned(page)
        || !arena.check_aligned(2 * block)
        || metadata.data().checked_add(3 * page)? > arena.data()
    {
        return None;
    }
    let storage = metadata + page;
    let base = arena.data();
    let mut valid = true;

    // The first, ordinary region has a consumed page. The low-address region
    // is appended by the architecture. Applying offset after sorting would
    // trim the wrong region and wrongly authorize a complete second block.
    let mut offset = [area(base + block, block), area(base, block)];
    let used = PlacementMap::normalize::<MMArch>(&mut offset, page)?;
    valid &= used == 2
        && offset[0].base == arena
        && offset[0].size == block
        && offset[1].base.data() == base + block + page
        && offset[1].size == block - page;
    let (spans, blocks, _) = PlacementMap::requirements(&offset[..used])?;
    valid &= spans == 1 && blocks == 1;
    let mut map = initialize(storage, &offset[..used])?;
    map.set::<MMArch>(arena, BLOCK_ORDER, FramePlacement::Movable);
    valid &= map.get::<MMArch>(arena) == FramePlacement::Movable
        && map.get::<MMArch>(arena + block) == FramePlacement::Unmovable;

    let mut adjacent = [area(base + block, block), area(base, block)];
    let used = PlacementMap::normalize::<MMArch>(&mut adjacent, 0)?;
    valid &= used == 1 && adjacent[0].base == arena && adjacent[0].size == 2 * block;
    let mut map = initialize(storage, &adjacent[..used])?;
    map.set::<MMArch>(arena, BLOCK_ORDER + 1, FramePlacement::Movable);
    valid &= map.get::<MMArch>(arena) == FramePlacement::Movable
        && map.get::<MMArch>(arena + block) == FramePlacement::Movable
        && map.get::<MMArch>(arena + 2 * block) == FramePlacement::Unmovable;

    let mut hole = [area(base, block), area(base + 2 * block, block)];
    let used = PlacementMap::normalize::<MMArch>(&mut hole, 0)?;
    let (spans, blocks, _) = PlacementMap::requirements(&hole[..used])?;
    valid &= used == 2 && spans == 2 && blocks == 2;
    let mut map = initialize(storage, &hole[..used])?;
    map.set::<MMArch>(arena, BLOCK_ORDER, FramePlacement::Movable);
    map.set::<MMArch>(arena + 2 * block, BLOCK_ORDER, FramePlacement::Movable);
    valid &= map.get::<MMArch>(arena) == FramePlacement::Movable
        && map.get::<MMArch>(arena + block) == FramePlacement::Unmovable
        && map.get::<MMArch>(arena + 2 * block) == FramePlacement::Movable;

    let mut partial = [area(base + page, 3 * block - 2 * page)];
    let used = PlacementMap::normalize::<MMArch>(&mut partial, 0)?;
    let (spans, blocks, _) = PlacementMap::requirements(&partial[..used])?;
    valid &= spans == 1 && blocks == 1;
    let mut map = initialize(storage, &partial[..used])?;
    map.set::<MMArch>(arena + block, BLOCK_ORDER, FramePlacement::Movable);
    valid &= map.get::<MMArch>(arena + page) == FramePlacement::Unmovable
        && map.get::<MMArch>(arena + block) == FramePlacement::Movable
        && map.get::<MMArch>(arena + 3 * block - 2 * page) == FramePlacement::Unmovable;

    // Pure addresses suffice here: no memory at the DMA32 boundary is read.
    let dma32 = 1usize << 32;
    let across_zone = [area(dma32 - block, 2 * block)];
    let (spans, blocks, _) = PlacementMap::requirements(&across_zone)?;
    valid &= spans == 2 && blocks == 2;
    let no_complete_block = [area(base + page, page)];
    valid &= PlacementMap::requirements(&no_complete_block)? == (0, 0, 0);
    let map = initialize(storage, &no_complete_block)?;
    valid &= map.get::<MMArch>(arena + page) == FramePlacement::Unmovable;
    for flags in [
        MemoryAreaAttr::RESERVED,
        MemoryAreaAttr::NOMAP,
        MemoryAreaAttr::DRIVER_MANAGED,
    ] {
        let mut forbidden = [PhysMemoryArea::new(arena, block, flags)];
        valid &= PlacementMap::normalize::<MMArch>(&mut forbidden, 0).is_none();
    }
    let mut overlap = [area(base, 2 * block), area(base + block, block)];
    valid &= PlacementMap::normalize::<MMArch>(&mut overlap, 0).is_none();
    let mut duplicate = [area(base, block), area(base, block)];
    valid &= PlacementMap::normalize::<MMArch>(&mut duplicate, 0).is_none();
    let mut overflow = [area(usize::MAX - page + 1, 2 * page)];
    valid &= PlacementMap::requirements(&overflow).is_none()
        && PlacementMap::normalize::<MMArch>(&mut overflow, 0).is_none();
    let mut unaligned_offset = [area(base, block)];
    valid &= PlacementMap::normalize::<MMArch>(&mut unaligned_offset, 1).is_none();
    let mut offset_overflow = [area(base, block)];
    valid &= PlacementMap::normalize::<MMArch>(&mut offset_overflow, usize::MAX).is_none();

    // All layout refusals happen before the first store. Check the entire
    // would-be output page and independent sentinels on both sides.
    let bytes = MMArch::phys_2_virt(storage)?.as_ptr::<u8>();
    core::ptr::write_bytes(bytes, 0xa5, page);
    MMArch::write::<u64>(MMArch::phys_2_virt(metadata)?, 0x2650_1111_2222_3333);
    MMArch::write::<u64>(MMArch::phys_2_virt(storage + page)?, 0x2650_4444_5555_6666);
    let layout = [area(base, 2 * block)];
    let (spans, blocks, required) = PlacementMap::requirements(&layout)?;
    valid &=
        PlacementMap::initialize::<MMArch>(storage, required - 1, spans, blocks, &layout).is_none();
    valid &= PlacementMap::initialize::<MMArch>(storage, page, 0, blocks, &layout).is_none();
    valid &=
        PlacementMap::initialize::<MMArch>(storage, page, spans, blocks - 1, &layout).is_none();
    valid &=
        PlacementMap::initialize::<MMArch>(storage + 1, page, spans, blocks, &layout).is_none();
    valid &= core::slice::from_raw_parts(bytes, page)
        .iter()
        .all(|byte| *byte == 0xa5)
        && MMArch::read::<u64>(MMArch::phys_2_virt(metadata)?) == 0x2650_1111_2222_3333
        && MMArch::read::<u64>(MMArch::phys_2_virt(storage + page)?) == 0x2650_4444_5555_6666;
    Some(valid)
}

pub(super) fn run(metadata: PhysAddr, arena: PhysAddr) -> bool {
    // SAFETY: the enclosing placement report retains the exclusive physical
    // backing through this call. No map or reference to it escapes the check.
    unsafe { checks(metadata, arena).unwrap_or(false) }
}
