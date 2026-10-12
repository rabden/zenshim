use rand::Rng;

const SORTABLE_CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// OpenCode SortableID (packages/schema/src/identifier.ts):
/// 26 chars = 12 hex of the top 48 bits of (ts * 0x1000 + counter),
/// bitwise-inverted for the descending variant, + 14 random base62.
pub fn sortable_id(descending: bool) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let counter = if LAST_MS.swap(now, Ordering::SeqCst) == now {
        COUNTER.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        COUNTER.store(0, Ordering::SeqCst);
        1
    };

    let current = (now as u128) * 0x1000 + counter as u128;
    let value = if descending { !current } else { current };
    let top48 = ((value >> 16) & 0xFFFF_FFFF_FFFF) as u64;
    let mut id = format!("{top48:012x}");

    let mut rng = rand::rng();
    for _ in 0..14 {
        let byte: u8 = rng.random();
        id.push(SORTABLE_CHARS[(byte % 62) as usize] as char);
    }
    id
}

pub fn session_id() -> String {
    format!("ses_{}", sortable_id(true))
}

pub fn request_id() -> String {
    format!("msg_{}", sortable_id(false))
}
