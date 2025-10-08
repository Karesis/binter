use std::{alloc::dealloc, ptr, sync::atomic::{AtomicPtr, AtomicUsize, Ordering as SyncOrdering}, usize::MAX};
use std::iter;
use std::sync::Mutex;
use std::ptr::NonNull;
use core::cmp::Ordering as MemOrdering;
use std::alloc::{Layout, alloc};
use std::mem;
use std::fmt;
use std::slice;
use super::chunkfooter::{ChunkFooter, EMPTY_CHUNK};

/// 常見系統的頁大小
/// 申請大塊內存時以頁為單位可以加速malloc
const TYPICAL_PAGE_SIZE: usize = 0x1000;
/// malloc申請內存的額外開銷
/// 會用於記錄這塊申請的元數據
const MALLOC_OVERHEAD: usize = 16;

/// chunk統一設計為使用16字節對齊
/// 滿足常用類型同時簡化計算
const SUPPORTED_ITER_ALIGNMENT: usize = 16;
const CHUNK_ALIGN: usize = SUPPORTED_ITER_ALIGNMENT;
/// chunkfooter結構體的大小
const FOOTER_SIZE: usize = mem::size_of::<ChunkFooter>();

/// 一個編譯期斷言，是一個保護設計
/// 確保上面的設計是有效的，
/// 防止意外修改chunkfooter帶來的對齊破壞
/// 比如有人向chunkfooter中加入32字節對齊的simd
const _FOOTER_ALIGN_ASSERTION: () = {
    assert!(mem::align_of::<ChunkFooter>() <= CHUNK_ALIGN);
};

/// 計算總的chunk的額外開銷
/// 由malloc需要使用的元數據開銷 + 自己（chunkfooter結構體大小）據開銷組成
/// 直接向上取整到16字節對齊即可
const OVERHEAD: usize = match round_up_to(MALLOC_OVERHEAD + FOOTER_SIZE, CHUNK_ALIGN) {
    Some(x) => x,
    None => panic!(),
};

/// 為第一個真實chunk設計的目標總大小（估算）
/// 是一個經驗值，測試出來的，可以平衡內存大小和性能
const FIRST_ALLOCATION_GOAL: usize = 1 << 9;

/// 用戶在第一個chunk中真正可用空間大小
/// 很簡單，就是設計上的第一次大小減去開銷即可
const DEFAULT_CHUNK_SIZE_WITHOUT_FOOTER: usize = FIRST_ALLOCATION_GOAL - OVERHEAD;


/// 線程安全的 Bump Allocator
/// 編譯器可以自動推斷出這個結構體是Sync的，
/// 同時就會自動滿足Send的條件
#[derive(Debug)]
pub struct SyncBump<const MIN_ALIGN: usize = 1> {
    /// 原子地指向當前活躍的 Chunk。
    current_chunkfooter: AtomicPtr<ChunkFooter>,

    /// 分配上限。`usize::MAX` 表示無上限。
    allocation_limit: AtomicUsize,
    
    /// 用於慢速路徑（分配新Chunk）的鎖，防止多個線程同時分配新Chunk。
    slow_path_lock: std::sync::Mutex<()>,
}

impl<const MIN_ALIGN: usize> Default for SyncBump<MIN_ALIGN> {
    fn default() -> Self {
        Self::with_min_align()
    }
}

/// 由於這個SyncBump是為全局線程安全的Interner特質的，
/// 所以drop只有在編譯結束後才會被調用。這裡
/// 使用Ordering::SeqCst是可以接受的。
impl<const MIN_ALIGN: usize> Drop for SyncBump<MIN_ALIGN> {
    fn drop(&mut self) {
        unsafe {
            dealloc_chunk_list(
                self
                    .current_chunkfooter
                    .load(SyncOrdering::SeqCst)
            );
        }
    }
}

/// 經典的鍊表釋放算法，只是這裡用Rust風格的函數式來書寫
/// ```
/// void dealloc_chunk_list(ChunkFooter* footer) {
///     while (footer != NULL && !is_empty(footer)) {
///         ChunkFooter* prev = footer->prev; // 1. 保存下一個節點
///         free(footer->data); // 2. 釋放當前節點的數據
///         footer = prev;      // 3. 移動到下一個節點
///     }
/// }
/// ```
unsafe fn dealloc_chunk_list(footer: *mut ChunkFooter) {
    let mut current_ptr = footer;

    let iterator = iter::from_fn(move || {
        let current_ref = unsafe { &*current_ptr };
        if current_ref.is_empty() {
            current_ptr = ptr::null_mut();
            return None;
        }
        let ptr_to_return = current_ptr;
        current_ptr = current_ref.prev.as_ptr();
        Some(ptr_to_return)
    });

    iterator.for_each(|chunk_ptr| {
        let chunk_ref = unsafe { &*chunk_ptr };
        unsafe {
            dealloc(chunk_ref.bottom.as_ptr(), chunk_ref.layout);
        }
    });
}

impl<const MIN_ALIGN: usize> SyncBump<MIN_ALIGN>  {
    /// 創建一個強制使用 MIN_ALIGN 作為最小對齊的 SyncBump。
    /// 對於非預設對齊的情況，必須使用此建構子。
    pub fn with_min_align() -> Self {
        // 執行與原版相同的斷言檢查
        assert!(MIN_ALIGN.is_power_of_two());
        assert!(MIN_ALIGN <= CHUNK_ALIGN); // CHUNK_ALIGN 是您內部定義的常量

        // 您的初始化邏輯
        SyncBump {
            current_chunkfooter: AtomicPtr::new(unsafe { EMPTY_CHUNK.get_ptr() }),
            allocation_limit: AtomicUsize::new(usize::MAX),
            slow_path_lock: Mutex::new(()),
        }
    }

    // 指定初始化大小的初始化
    pub fn with_capacity(capacity: usize) -> Self {
        Self::try_with_capacity(capacity).unwrap_or_else(|_| oom())
    }

    pub fn try_with_capacity(capacity: usize) -> Result<Self, AllocErr> {
        Self::try_with_min_align_and_capacity(capacity)
    }

    pub fn try_with_min_align_and_capacity(capacity: usize) -> Result<Self, AllocErr> {
        assert!(
            MIN_ALIGN.is_power_of_two(),
            "MIN_ALIGN must be a power of two; found {MIN_ALIGN}"
        );
        assert!(
            MIN_ALIGN <= CHUNK_ALIGN,
            "MIN_ALIGN may not be larger than {CHUNK_ALIGN}; found {MIN_ALIGN}"
        );

        if capacity == 0 {
            return Ok(SyncBump {
                current_chunkfooter: AtomicPtr::new(unsafe { EMPTY_CHUNK.get_ptr() }),
                allocation_limit: AtomicUsize::new(MAX),
                slow_path_lock: Mutex::new(()),
            });
        }

        let layout = layout_from_size_align(capacity, MIN_ALIGN)?;

        let chunk_footer = unsafe {
            Self::new_chunk(
                Self::new_chunk_memory_details(None, layout).ok_or(AllocErr)?,
                layout,
                NonNull::new_unchecked(EMPTY_CHUNK.get_ptr()),
            )
            .ok_or(AllocErr)?
        };

        Ok(SyncBump {
            current_chunkfooter: AtomicPtr::new(chunk_footer.as_ptr()),
            allocation_limit: AtomicUsize::new(MAX),
            slow_path_lock: Mutex::new(())
        })
    }

    // 驻留一个字符串
    #[inline(always)]
    pub fn alloc_str(&self, src: &str) -> &mut str {
        let buffer = self.alloc_slice_copy(src.as_bytes());
        unsafe {
            // This is OK, because it already came in as str, so it is guaranteed to be utf8
            str::from_utf8_unchecked_mut(buffer)
        }
    }

    #[inline(always)]
    pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        self.try_alloc_layout(layout).unwrap_or_else(|_| oom())
    }

    #[inline(always)]
    pub fn try_alloc_layout(&self, layout: Layout) -> Result<NonNull<u8>, AllocErr> {
        if let Some(p) = self.try_alloc_layout_fast(layout) {
            Ok(p)
        } else {
            self.alloc_layout_slow(layout).ok_or(AllocErr)
        }
    }

    fn try_alloc_layout_fast(
        &self,
        layout: Layout,
    ) -> Option<NonNull<u8>> {
        // 步驟 1: 原子性地讀取當前 chunk 的指針。
        // 使用 Acquire 來確保如果我們看到了新 chunk，那麼它的內容也對我們可見。
        let chunk_ptr = self.current_chunkfooter.load(SyncOrdering::Acquire);
        
        // 如果是哨兵 chunk，它沒有容量，直接走慢速路徑。
        if unsafe { (*chunk_ptr).is_empty() } {
            return None;
        }

        let chunk_ref = unsafe { &*chunk_ptr };
        let start = chunk_ref.bottom.as_ptr(); 

        // --- CAS 迴圈開始 ---
        // 這是一個樂觀鎖：我們樂觀地認為我們可以在沒有衝突的情況下完成分配。
        // 如果失敗了（有其他線程搶先了），我們就重試。
        loop {
            // 步驟 2: 原子性地讀取 bump 指針的當前值 ("old" value)。
            // 每次循環都必須重新讀取，因為它可能已經被其他線程改變。
            let ptr = chunk_ref.top.load(SyncOrdering::Acquire);

            // 步驟 3: 執行與原版完全相同的計算邏輯。
            // 這部分是純計算，不涉及共享狀態的修改，所以可以直接複用。
            let aligned_ptr = match layout.align().cmp(&MIN_ALIGN) {
                MemOrdering::Less => {
                    let aligned_size = round_up_to(layout.size(), MIN_ALIGN)?;
                    if aligned_size > (ptr as usize).wrapping_sub(start as usize) {
                        return None; // 容量不足，需要走慢速路徑
                    }
                    ptr.wrapping_sub(aligned_size)
                }
                MemOrdering::Equal => {
                    let aligned_size = unsafe { round_up_to_unchecked(layout.size(), layout.align()) };
                    if aligned_size > (ptr as usize).wrapping_sub(start as usize) {
                        return None;
                    }
                    ptr.wrapping_sub(aligned_size)
                }
                MemOrdering::Greater => {
                    let aligned_size = unsafe { round_up_to_unchecked(layout.size(), layout.align()) };
                    let aligned_ptr_candidate = round_mut_ptr_down_to(ptr, layout.align());
                    if aligned_ptr_candidate < start 
                    || aligned_size > (aligned_ptr_candidate as usize).wrapping_sub(start as usize) {
                        return None;
                    }
                    aligned_ptr_candidate.wrapping_sub(aligned_size)
                }
            };

            // 步驟 4: 執行 CAS 操作 (Compare-and-Exchange)。
            // 這是原子性的「讀-改-寫」。
            // 我們嘗試將 `top` 指針從我們剛剛讀到的 `ptr` 值，更新為我們計算出的 `aligned_ptr` 值。
            match chunk_ref
                .top
                .compare_exchange(
                ptr,                     // 我們期望的當前值 (if top == ptr)
                aligned_ptr,                 // 我們想要寫入的新值 (then top = aligned_ptr)
                SyncOrdering::AcqRel,    // 成功時的 ordering
                SyncOrdering::Acquire,   // 失敗時的 ordering
                ) {
                    Ok(_) => {
                        // 成功！沒有其他線程在我們計算期間修改 `top` 指針。
                        // 我們成功地 "搶" 到了這塊內存。
                        // `aligned_ptr` 現在指向我們分配到的內存的起始位置。
                        return Some(unsafe { NonNull::new_unchecked(aligned_ptr) });
                    }
                    Err(_) => {
                        // 失敗！有其他線程搶先修改了 `top` 指針。
                        // `compare_exchange` 失敗了，但它不會做任何修改。
                        // 我們什麼都不用做，直接進入下一次 `loop`，
                        // 使用被更新過的 `top` 值重新開始我們的計算。
                        continue;
                    }
                }
        }
    }

    #[inline(never)]
    #[cold]
    fn alloc_layout_slow(
        &self,
        layout: Layout,
    ) -> Option<NonNull<u8>> {
        // 步驟 1: 獲取鎖，以獨占的方式進入慢速路徑。
        // 任何時候，只有一個線程能拿到鎖並繼續執行。
        // 其他線程會在這裡等待。
        let _guard = self.slow_path_lock.lock().unwrap();

        // 步驟 2: 雙重檢查 (Double-Checked)
        // 在我們等待鎖的期間，可能已經有另一個線程為我們分配好了一個新的 Chunk。
        // 所以，在真正執行分配前，我們必須再試一次快速路徑！
        if let Some(ptr) = self.try_alloc_layout_fast(layout) {
            // 如果這次成功了，說明問題已經被解決，我們無需再做任何事。
            return Some(ptr);
        }

        // 步驟 3: 如果快速路徑再次失敗，說明輪到我們來解決問題了。
        // 現在可以安全地執行原版的慢速路徑邏輯。
        unsafe {
            let allocation_limit_remaining = self.allocation_limit_remaining();

            // Get a new chunk from the global allocator.
            let current_chunk_ptr = self.current_chunkfooter.load(SyncOrdering::Acquire);
            let current_chunk_ref = &*current_chunk_ptr;
            let current_layout = current_chunk_ref.layout;

            // By default, we want our new chunk to be about twice as big
            // as the previous chunk. If the global allocator refuses it,
            // we try to divide it by half until it works or the requested
            // size is smaller than the default footer size.
            let min_new_chunk_size = layout.size().max(DEFAULT_CHUNK_SIZE_WITHOUT_FOOTER);
            let mut base_size = (current_layout.size() - FOOTER_SIZE)
                .checked_mul(2)?
                .max(min_new_chunk_size);
            let chunk_memory_details = iter::from_fn(|| {
                let bypass_min_chunk_size_for_small_limits = matches!(self.allocation_limit(), Some(limit) if layout.size() < limit
                            && base_size >= layout.size()
                            && limit < DEFAULT_CHUNK_SIZE_WITHOUT_FOOTER
                            && self.allocated_bytes() == 0);

                if base_size >= min_new_chunk_size || bypass_min_chunk_size_for_small_limits {
                    let size = base_size;
                    base_size /= 2;
                    Self::new_chunk_memory_details(Some(size), layout)
                } else {
                    None
                }
            });

            let new_footer_ptr = chunk_memory_details
                .filter_map(|chunk_memory_details| {
                    if Self::chunk_fits_under_limit(
                        allocation_limit_remaining,
                        chunk_memory_details,
                    ) {
                        let prev_non_null = NonNull::new(current_chunk_ptr)
                            .expect("BUG: current_chunk pointer should never be null.");
                        Self::new_chunk(chunk_memory_details, layout, prev_non_null)
                    } else {
                        None
                    }
                })
                .next()?;

            debug_assert_eq!(
                new_footer_ptr.as_ref().bottom.as_ptr() as usize % layout.align(),
                0
            );

            // 步驟 4: 安全地發布 (Publish) 新的 Chunk
            // 使用 store 和 Release 語義，將新的 Chunk 地址原子性地更新到全局指針。
            // 這一步確保了其他所有線程都能看到這個全新的、初始化完整的 Chunk。
            self.current_chunkfooter.store(new_footer_ptr.as_ptr(), SyncOrdering::Release);

            // And then we can rely on `tray_alloc_layout_fast` to allocate
            // space within this chunk.
            let ptr = self.try_alloc_layout_fast(layout);
            debug_assert!(ptr.is_some());
            ptr
        }
    }

    // 深入探討：關於「快照」一致性
    // 這裡有一個非常微妙的並行問題。在 allocation_limit_remaining 中，我們執行了兩次原子讀取：
    //     讀取 allocation_limit
    //     （在 allocated_bytes 內部）讀取 current_chunk
    // 這兩次 load 操作之間，CPU 可能會切換線程。這意味著我們讀到的 limit 和 allocated 可能來自於兩個略微不同的時間點。比如：
    //     線程 A 讀到 limit = 10000。
    //     線程 B 介入，將 limit 設為 20000，並分配了一個巨大的新 chunk，使 allocated_bytes 變得很大。
    //     線程 A 繼續執行，讀到一個很大的 allocated_bytes。
    //     線程 A 可能會用一個舊的 limit (10000) 和一個新的 allocated (例如 15000) 進行比較，得出一個錯誤的結論（比如空間不足）。
    // 這是一個良性的數據競爭 (Benign Race Condition)。
    //     為什麼是良性的？ 因為它不會破壞內存安全。最壞的情況是，我們的慢速路徑做出了一個略微過時的、次優的決策（比如，因為讀到了不一致的狀態而錯誤地認為內存不足，導致本次分配失敗）。但它絕不會導致程序崩潰或數據損壞。
    //     為什麼我們可以接受它？ 因為要獲取一個完全同步的原子性快照 (snapshot)，唯一的辦法就是使用一個更大的全局鎖，而這正是我們極力避免的。對於慢速路徑中的一個輔助決策，這種微小的不一致性是可以接受的成本。
    // 結論：對於這些輔助函數，我們的策略就是使用帶有 Acquire 語義的原子讀取來獲取狀態的「近期快照」。這足以保證內存安全和程序的穩健運行。您已經成功地將它們改造完成了！
    fn allocation_limit_remaining(&self) -> Option<usize> {
        // 步驟 1: 原子性地 `load` 分配上限值
        // 因為這個值只是一個數字，不與其他數據結構的初始化掛鉤，
        // 使用 Relaxed 理論上是可行的。但為了安全和一致性，Acquire 是更穩妥的選擇，
        // 在冷路徑中，這點性能差異可以忽略不計。
        let limit = self.allocation_limit.load(SyncOrdering::Acquire);

        // 我們用 usize::MAX 作為哨兵值來代表 `None`
        if limit == usize::MAX {
            return None;
        }

        // 步驟 2: 調用我們剛才改造好的、線程安全的 `allocated_bytes`
        let allocated = self.allocated_bytes();
        
        // 步驟 3: 執行純計算
        if allocated > limit {
            None
        } else {
            Some(limit - allocated) // usize::abs_diff in new versions
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        // 步驟 1: 原子性地 `load` 當前 chunk 的指針
        // 我們使用 `Acquire` 語義，因為我們接下來要讀取這個指針指向的內存。
        // 這確保我們能看到一個被 `Release` 發布的、完全初始化的 ChunkFooter。
        let footer_ptr = self.current_chunkfooter.load(SyncOrdering::Acquire);

        // 步驟 2: 安全地讀取字段
        // `allocated_bytes` 字段本身是不可變的，一旦 chunk 被創建就不會改變。
        // 所以，只要我們拿到了指向它的有效指針，直接讀取就是安全的。
        // `unsafe` 是因為我們需要解引用裸指針。
        unsafe {
            // 我們可以斷言指針不為空，因為它總是指向某個 chunk (至少是哨兵節點)
            debug_assert!(!footer_ptr.is_null());
            (*footer_ptr).allocated_bytes
        }
    }

    /// 這裡使用Acquire,因為假設未來我添加了動態調整allocation_limit
    /// 那麼這裡就會需要“acquire 一個 release"
    /// 且對於”冷路徑“，Relaxed帶來的性能提升有限
    pub fn allocation_limit(&self) -> Option<usize> {
        match self.allocation_limit.load(SyncOrdering::Acquire) {
            usize::MAX => None,
            limit => Some(limit)
        }
    }

    fn new_chunk_memory_details(
        new_size_without_footer: Option<usize>,
        requested_layout: Layout,
    ) -> Option<NewChunkMemoryDetails> {
        // 計算新的對齊要求
        // 這會在預先定義好的chunk對齊要求（16字節）
        // 和bump創建時候指定的對齊要求
        // 以及這個函數調用所請求的align要求（requested）
        // 之間取得一個最大值
        let align = CHUNK_ALIGN
            .max(MIN_ALIGN)
            .max(requested_layout.align());

        // 如果提供了就用提供的，沒有就用默認的
        let mut new_size_without_footer =
            new_size_without_footer.unwrap_or(DEFAULT_CHUNK_SIZE_WITHOUT_FOOTER);
        
        // 這裡也是一個“取最大”的操作
        // 確保要求的和默認的同時滿足
        let requested_size =
            round_up_to(requested_layout.size(), align).unwrap_or_else(allocation_size_overflow);
        new_size_without_footer = new_size_without_footer.max(requested_size);

        // 這裡利用“預估”的內存頁大小做計算
        // 如果小於一個頁大小，那麼就湊成2的冪次方
        // 這樣可以加速處理
        // 如果大於一個頁，那麼就優先湊成頁的整數倍
        // 這個設計也是利用了內存利用和時間效率的平衡
        if new_size_without_footer < TYPICAL_PAGE_SIZE {
            new_size_without_footer =
                (new_size_without_footer + OVERHEAD).next_power_of_two() - OVERHEAD;
        } else {
            new_size_without_footer =
                round_up_to(new_size_without_footer + OVERHEAD, TYPICAL_PAGE_SIZE)? - OVERHEAD;
        }

        // 宏用於確保安全
        debug_assert_eq!(align % CHUNK_ALIGN, 0);
        debug_assert_eq!(new_size_without_footer % CHUNK_ALIGN, 0);

        // 組裝
        let size = new_size_without_footer
            .checked_add(FOOTER_SIZE)
            .unwrap_or_else(allocation_size_overflow);

        // 返回
        Some(NewChunkMemoryDetails {
            new_size_without_footer,
            size,
            align,
        })
    }

    unsafe fn new_chunk(
        new_chunk_memory_details: NewChunkMemoryDetails,
        requested_layout: Layout,
        prev: NonNull<ChunkFooter>,
    ) -> Option<NonNull<ChunkFooter>> {
        let NewChunkMemoryDetails {
            new_size_without_footer,
            align,
            size,
        } = new_chunk_memory_details;

        let layout = layout_from_size_align(size, align).ok()?;

        debug_assert!(size >= requested_layout.size());

        let bottom = unsafe { alloc(layout) };
        let bottom = NonNull::new(bottom)?;

        // The `ChunkFooter` is at the end of the chunk.
        let footer_ptr = unsafe { bottom.as_ptr().add(new_size_without_footer) };
        debug_assert_eq!((bottom.as_ptr() as usize) % align, 0);
        debug_assert_eq!(footer_ptr as usize % CHUNK_ALIGN, 0);
        let footer_ptr = footer_ptr as *mut ChunkFooter;

        // The bump pointer is initialized to the end of the range we will bump
        // out of, rounded down to the minimum alignment. It is the
        // `NewChunkMemoryDetails` constructor's responsibility to ensure that
        // even after this rounding we have enough non-zero capacity in the
        // chunk.
        let ptr = round_mut_ptr_down_to(footer_ptr.cast::<u8>(), MIN_ALIGN);
        debug_assert_eq!(ptr as usize % MIN_ALIGN, 0);
        debug_assert!(
            bottom.as_ptr() < ptr,
            "bump pointer {ptr:#p} should still be greater than or equal to the \
             start of the bump chunk {bottom:#p}"
        );
        debug_assert_eq!(
            (ptr as usize) - (bottom.as_ptr() as usize),
            new_size_without_footer
        );

        let top = AtomicPtr::new(ptr);

        // The `allocated_bytes` of a new chunk counts the total size
        // of the chunks, not how much of the chunks are used.
        let allocated_bytes = unsafe { prev.as_ref().allocated_bytes + new_size_without_footer };
        unsafe {
            ptr::write(
                footer_ptr,
                ChunkFooter {
                    bottom,
                    layout,
                    prev,
                    top,
                    allocated_bytes,
                },
            )
        };

        Some(unsafe {
            NonNull::new_unchecked(footer_ptr)
        })
    }

    /// Whether a request to allocate a new chunk with a given size for a given
    /// requested layout will fit under the allocation limit set on a `Bump`.
    fn chunk_fits_under_limit(
        allocation_limit_remaining: Option<usize>,
        new_chunk_memory_details: NewChunkMemoryDetails,
    ) -> bool {
        allocation_limit_remaining
            .map(|allocation_limit_left| {
                allocation_limit_left >= new_chunk_memory_details.new_size_without_footer
            })
            .unwrap_or(true)
    }

    #[inline(always)]
    pub fn alloc_slice_copy<T>(&self, src: &[T]) -> &mut [T]
    where
        T: Copy,
    {
        let layout = Layout::for_value(src);
        let dst = self.alloc_layout(layout).cast::<T>();

        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), src.len());
            slice::from_raw_parts_mut(dst.as_ptr(), src.len())
        }
    }
}

/// The memory size and alignment details for a potential new chunk
/// allocation.
#[derive(Debug, Clone, Copy)]
struct NewChunkMemoryDetails {
    new_size_without_footer: usize,
    align: usize,
    size: usize,
}

#[cold]
#[inline(never)]
fn allocation_size_overflow<T>() -> T {
    panic!("requested allocation size overflowed")
}

/// Wrapper around `Layout::from_size_align` that adds debug assertions.
#[inline]
fn layout_from_size_align(size: usize, align: usize) -> Result<Layout, AllocErr> {
    Layout::from_size_align(size, align).map_err(|_| AllocErr)
}

#[inline]
fn is_pointer_aligned_to<T>(pointer: *mut T, align: usize) -> bool {
    debug_assert!(align.is_power_of_two());

    let pointer = pointer as usize;
    let pointer_aligned = round_down_to(pointer, align);
    pointer == pointer_aligned
}

#[inline]
const fn round_up_to(n: usize, divisor: usize) -> Option<usize> {
    debug_assert!(divisor > 0);
    debug_assert!(divisor.is_power_of_two());
    match n.checked_add(divisor - 1) {
        Some(x) => Some(x & !(divisor - 1)),
        None => None,
    }
}

/// Like `round_up_to` but turns overflow into undefined behavior rather than
/// returning `None`.
#[inline]
unsafe fn round_up_to_unchecked(n: usize, divisor: usize) -> usize {
    match round_up_to(n, divisor) {
        Some(x) => x,
        None => {
            debug_assert!(false, "round_up_to_unchecked failed");
            unsafe { core::hint::unreachable_unchecked() }
        }
    }
}

#[inline]
fn round_down_to(n: usize, divisor: usize) -> usize {
    debug_assert!(divisor > 0);
    debug_assert!(divisor.is_power_of_two());
    n & !(divisor - 1)
}

/// Same as `round_down_to` but preserves pointer provenance.
#[inline]
fn round_mut_ptr_down_to(ptr: *mut u8, divisor: usize) -> *mut u8 {
    debug_assert!(divisor > 0);
    debug_assert!(divisor.is_power_of_two());
    ptr.wrapping_sub(ptr as usize & (divisor - 1))
}

#[inline]
unsafe fn round_mut_ptr_up_to_unchecked(ptr: *mut u8, divisor: usize) -> *mut u8 {
    debug_assert!(divisor > 0);
    debug_assert!(divisor.is_power_of_two());
    let aligned = unsafe { round_up_to_unchecked(ptr as usize, divisor) };
    let delta = aligned - (ptr as usize);
    unsafe { ptr.add(delta) }
}

#[inline(never)]
#[cold]
fn oom() -> ! {
    panic!("out of memory")
}

/// The `AllocErr` error indicates an allocation failure
/// that may be due to resource exhaustion or to
/// something wrong when combining the given input arguments with this
/// allocator.
// #[unstable(feature = "allocator_api", issue = "32838")]
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AllocErr;

// (we need this for downstream impl of trait Error)
// #[unstable(feature = "allocator_api", issue = "32838")]
impl fmt::Display for AllocErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("memory allocation failed")
    }
}