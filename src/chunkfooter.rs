use std::alloc::Layout; 
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, Ordering};

#[repr(C)]
#[derive(Debug)]
pub(super) struct ChunkFooter {
    /// 指向此 Chunk 可用內存區域的起始地址。
    /// 一旦設定後不可變。
    pub(super) bottom: NonNull<u8>,

    /// 此 Chunk 的內存佈局（大小和對齊）。
    /// 一旦設定後不可變。
    pub(super) layout: Layout,

    /// 指向前一個 Chunk。用於遍歷和釋放。
    /// 一旦設定後不可變。
    pub(super) prev: NonNull<ChunkFooter>,

    /// 原子指針，指向當前可分配內存的頂部（或底部，取決於分配方向）。
    /// 這是性能熱點，所有分配操作都會對其進行 CAS 操作。
    pub(super) top: AtomicPtr<u8>,

    /// 此 Chunk 及其所有 `prev` Chunks 的總大小。
    /// 在創建時計算，之後不可變。
    pub(super) allocated_bytes: usize,
}

/// 一個空的ChunkFooter,用於初始化
/// 這樣設計可以在初始化時不立即分配內存
#[repr(transparent)]
pub(super) struct EmptyChunkFooter(ChunkFooter);

/// 這是安全的，因為這只會用於一個全局靜態的常量
unsafe impl Sync for EmptyChunkFooter {}

pub(super) static EMPTY_CHUNK: EmptyChunkFooter = EmptyChunkFooter(ChunkFooter { 
    bottom: unsafe {
        NonNull::new_unchecked(
            &EMPTY_CHUNK 
                as *const EmptyChunkFooter 
                as *mut u8
        )
    }, 

    layout: Layout::new::<ChunkFooter>(), 

    prev: unsafe {
        NonNull::new_unchecked(
            &EMPTY_CHUNK
                as *const EmptyChunkFooter
                as *mut ChunkFooter
        )
    }, 

    top: AtomicPtr::new(
        &EMPTY_CHUNK 
            as *const EmptyChunkFooter
            as *mut u8
    ), 

    allocated_bytes: 0
});

impl EmptyChunkFooter {
    pub(super) fn get(&'static self) -> AtomicPtr<ChunkFooter> {
        AtomicPtr::new(
            &self.0
                as *const ChunkFooter
                as *mut ChunkFooter
        )
    }

    /// 直接獲取指向內部 ChunkFooter 的裸指針。
    ///
    /// # Safety
    /// 這個函數是 `unsafe` 的，因為它涉及到 `const` 到 `mut` 的轉換。
    /// 但這是安全的，因為我們承諾絕不會通過這個指針修改靜態的 EMPTY_CHUNK。
    pub(super) unsafe fn get_ptr(&'static self) -> *mut ChunkFooter {
        // 直接將 self.0 的引用轉換為裸指針
        &self.0 as *const ChunkFooter as *mut ChunkFooter
    }
}

impl ChunkFooter {
    // 獲取當前chunk的指針位置（同時也是已分配內存的起始位置）
    // 和已分配內存大小
    fn get_current_top_and_allocated_size(
        &self
    ) -> (*const u8, usize) {
        let bottom = self.bottom.as_ptr() as *const u8;
        let top = self.top.load(Ordering::SeqCst) as *const u8;
        debug_assert!(bottom <= top);
        debug_assert!(top <= self as *const ChunkFooter as *const u8);
        let len = unsafe {
            (self 
                as *const ChunkFooter 
                as *const u8
            ).offset_from(top)
            as usize
        };
        (top, len)
    }

    // 判斷該chunk是否是一個空的chunk
    pub(super) fn is_empty(&self) -> bool {
        ptr::eq(self, EMPTY_CHUNK.get().load(Ordering::SeqCst))
    }
}