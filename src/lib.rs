mod chunkfooter;
mod syncbump;

use std::collections::HashMap;
use std::sync::RwLock;
use syncbump::SyncBump;

// --- 符號 (Symbol) 類型 ---
/// 一個輕量級的、唯一的字符串標識符。
/// 它可以被高效地複製、傳遞、比較和用作哈希表的鍵。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(u32);

// --- 內部數據結構 ---
/// 這個結構體包含了所有需要被鎖保護的共享數據。
/// 把它們放在一個單獨的結構體中，可以讓鎖的管理更清晰。
struct InternerData<'a> {
    /// 從 &str 快速查找到對應的 Symbol。
    map: HashMap<&'a str, Symbol>,
    /// 從 Symbol 快速查找到對應的 &str。
    /// Symbol 的 u32 值就是這個 Vec 的索引。
    vec: Vec<&'a str>,
}

// --- Interner 主結構體 ---
/// 一個線程安全的、基於 Bump Allocator 的全局字符串駐留池。
pub struct Interner<'bump, const MIN_ALIGN: usize = 1> {
    /// 底層的 Bump Allocator，負責實際的內存分配。
    arena: SyncBump<MIN_ALIGN>,
    /// 被讀寫鎖保護的核心數據（查找表）。
    data: RwLock<InternerData<'bump>>,
}

impl<'bump, const MIN_ALIGN: usize> Interner<'bump, MIN_ALIGN> {
    /// 創建一個帶有預設容量的 Interner，以提高性能。
    pub fn with_capacity(capacity: usize) -> Self {
        Interner {
            arena: SyncBump::<MIN_ALIGN>::with_capacity(capacity * 10), // 假設平均字符串長度為10
            data: RwLock::new(InternerData {
                map: HashMap::with_capacity(capacity),
                vec: Vec::with_capacity(capacity),
            }),
        }
    }

    /// 將一個字符串存入池中，返回其唯一的 Symbol。
    /// 如果字符串已存在，則返回現有的 Symbol；否則，會分配新內存並創建新的 Symbol。
    pub fn intern(&'bump self, s: &str) -> Symbol {
        // --- 快速讀取路徑 ---
        // 1. 獲取讀鎖，檢查字符串是否已存在。
        let read_guard = self.data.read().expect("RwLock poisoned during read");
        if let Some(symbol) = read_guard.map.get(s) {
            return *symbol;
        }
        drop(read_guard); // 顯式釋放讀鎖，為接下來的寫鎖做準備

        // --- 慢速寫入路徑 ---
        // 2. 獲取寫鎖。
        let mut write_guard = self.data.write().unwrap();

        // 3. 雙重檢查！在等待寫鎖時，可能已有其他線程完成了插入。
        if let Some(symbol) = write_guard.map.get(s) {
            return *symbol;
        }

        // 4. 確認沒有，執行真正的分配和插入。
        let id = write_guard.vec.len() as u32;
        let symbol = Symbol(id);

        // 使用 arena 分配一個生命週期為 'bump 的字符串
        let interned_str = self.arena.alloc_str(s);

        // 同時更新 vec 和 map，保持數據一致性
        write_guard.vec.push(interned_str);
        write_guard.map.insert(interned_str, symbol);

        symbol
    }

    /// 根據 Symbol，獲取其對應的字符串切片。
    /// 如果 Symbol 無效，返回 None。
    pub fn resolve(&self, symbol: Symbol) -> Option<&'bump str> {
        let read_guard = self.data.read().expect("RwLock poisoned during read");
        // 將 u32 索引轉換為 usize，並安全地訪問 Vec
        read_guard.vec.get(symbol.0 as usize).copied()
    }

    /// 返回池中獨立字符串的數量。
    pub fn len(&self) -> usize {
        self.data
            .read()
            .expect("RwLock poisoned during read")
            .vec
            .len()
    }

    /// 检查池中是否没有任何字符串。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 返回 Interner 底層 Arena 已分配的總內存字節數。
    pub fn memory_usage(&self) -> usize {
        self.arena.allocated_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;

    #[test]
    fn test_intern_and_resolve_basic() {
        // Interner 需要一个生命周期 'bump。在测试中，一个简单的方法是
        // 创建一个 static 的 Interner，这样它的生命周期就是 'static。
        // once_cell 是做这个的常用库。

        static INTERNER: Lazy<Interner<'static>> = Lazy::new(|| Interner::with_capacity(32));

        let s1 = "hello";
        let sym1 = INTERNER.intern(s1);

        // 验证解析回来的字符串和原始字符串相等
        assert_eq!(INTERNER.resolve(sym1), Some(s1));

        // 验证长度
        assert_eq!(INTERNER.len(), 1);
    }

    #[test]
    fn test_intern_uniqueness() {
        static INTERNER: Lazy<Interner<'static>> = Lazy::new(|| Interner::with_capacity(32));

        let s = "world";
        let sym1 = INTERNER.intern(s);
        let sym2 = INTERNER.intern(s);

        // 两次调用应该返回同一个 Symbol
        assert_eq!(sym1, sym2);

        // Interner 的长度应该只增加了一次
        assert_eq!(INTERNER.len(), 1);

        // 插入一个不同的字符串
        let s_new = "rust";
        let sym_new = INTERNER.intern(s_new);

        assert_ne!(sym1, sym_new);
        assert_eq!(INTERNER.len(), 2);
        assert_eq!(INTERNER.resolve(sym_new), Some(s_new));
    }

    #[test]
    fn test_edge_cases() {
        static INTERNER: Lazy<Interner<'static>> = Lazy::new(|| Interner::with_capacity(16));

        // 测试空字符串
        let empty_sym = INTERNER.intern("");
        assert_eq!(INTERNER.resolve(empty_sym), Some(""));
        assert_eq!(INTERNER.len(), 1);

        // 测试一个从未被创建的 Symbol
        let invalid_sym = Symbol(999);
        assert_eq!(INTERNER.resolve(invalid_sym), None);
    }

    #[test]
    fn test_concurrent_interning() {
        use std::thread;

        // 使用 static Lazy 确保所有线程都访问同一个 Interner 实例
        static INTERNER: Lazy<Interner<'static>> = Lazy::new(|| Interner::with_capacity(100));

        // 一些待测试的字符串，包含很多重复项
        let strings_to_intern = vec![
            "apple", "banana", "orange", "apple", "grape", "banana", "kiwi", "apple", "mango",
            "orange", "grape", "papaya",
        ];

        // 启动一个线程作用域
        thread::scope(|s| {
            // 创建10个线程
            for _ in 0..10 {
                // 每个线程都会尝试插入所有字符串
                s.spawn(|| {
                    for &string in &strings_to_intern {
                        // 调用我们正在测试的函数
                        INTERNER.intern(string);
                    }
                });
            }
        }); // scope 结束时，所有线程都已执行完毕并 join

        // --- 在所有并发操作结束后，验证最终状态 ---

        // 1. 验证最终的长度是否等于独立字符串的数量
        // 独立字符串: "apple", "banana", "orange", "grape", "kiwi", "mango", "papaya" -> 7个
        let unique_strings_count = 7;
        assert_eq!(INTERNER.len(), unique_strings_count);

        // 2. 验证任意一个字符串的唯一性
        // 即使在 10 个线程中被插入了 30 次，"apple" 也应该只有一个 Symbol
        let sym_apple1 = INTERNER.intern("apple");
        let sym_apple2 = INTERNER.intern("apple");
        assert_eq!(sym_apple1, sym_apple2);

        // 3. 验证所有独立字符串都能被正确解析
        assert_eq!(INTERNER.resolve(INTERNER.intern("apple")), Some("apple"));
        assert_eq!(INTERNER.resolve(INTERNER.intern("papaya")), Some("papaya"));
        assert_eq!(INTERNER.resolve(INTERNER.intern("banana")), Some("banana"));
    }
}
