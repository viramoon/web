use super::InvalidPagination;
use openssl::rand::rand_bytes;
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::{LazyLock, Mutex},
};

// Finance accepts every nonnegative i64 as an exclusive job position. Only a
// negative value can identify a web continuation without shadowing a real cursor.
const HANDLE_COUNT: i64 = 1 << 51;
pub(super) const HANDLE_RANGE: Range<i64> = -2 * HANDLE_COUNT..-HANDLE_COUNT;
const MAX_HANDLES: usize = 4096;

// Membership authenticates the handle. Its bits are independent of the scan position.
static HANDLES: LazyLock<Mutex<Handles>> = LazyLock::new(Mutex::default);

pub(super) fn issue(position: i64) -> Result<i64, ()> {
    for _ in 0..8 {
        let mut bytes = [0; 8];
        rand_bytes(&mut bytes).map_err(|_| ())?;
        let handle = HANDLE_RANGE.start + (i64::from_be_bytes(bytes) & (HANDLE_COUNT - 1));
        if HANDLES.lock().map_err(|_| ())?.insert(handle, position) {
            return Ok(handle);
        }
    }
    Err(())
}

pub(super) fn resolve(value: i64) -> Result<i64, InvalidPagination> {
    if value >= 0 {
        return Ok(value);
    }
    if !HANDLE_RANGE.contains(&value) {
        return Err(InvalidPagination::Parameters);
    }
    HANDLES
        .lock()
        .ok()
        .and_then(|store| store.positions.get(&value).copied())
        .ok_or(InvalidPagination::Cursor)
}

#[derive(Default)]
struct Handles {
    positions: HashMap<i64, i64>,
    issued: VecDeque<i64>,
}

impl Handles {
    fn insert(&mut self, handle: i64, position: i64) -> bool {
        if self.positions.contains_key(&handle) {
            return false;
        }
        if self.issued.len() == MAX_HANDLES {
            let oldest = self.issued.pop_front().expect("full handle queue");
            self.positions.remove(&oldest);
        }
        self.positions.insert(handle, position);
        self.issued.push_back(handle);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_cursor_is_randomized_and_rejects_an_unissued_neighbor() {
        let handle = issue(208).unwrap();
        assert!(HANDLE_RANGE.contains(&handle));
        assert_eq!(resolve(handle).unwrap(), 208);
        assert_ne!(handle, issue(208).unwrap());
        assert!(matches!(
            resolve(handle ^ 1),
            Err(InvalidPagination::Cursor)
        ));
        assert!(!Handles::default().positions.contains_key(&handle));
    }

    #[test]
    fn cursor_parser_keeps_legacy_nonnegative_decimal_input() {
        let boundaries = [1_i64 << 52, 1_i64 << 53];
        let neighbors = boundaries.into_iter().flat_map(|n| (n - 1)..=(n + 1));
        for id in [0, 7, i64::MAX].into_iter().chain(neighbors) {
            assert_eq!(resolve(id).unwrap(), id);
            if id > 0 {
                let handle = issue(id).unwrap();
                assert!(HANDLE_RANGE.contains(&handle));
                assert_eq!(resolve(handle).unwrap(), id);
                assert_eq!(resolve(id).unwrap(), id);
            }
        }
        for invalid in [i64::MIN, HANDLE_RANGE.start - 1, HANDLE_RANGE.end, -1] {
            assert!(matches!(
                resolve(invalid),
                Err(InvalidPagination::Parameters)
            ));
        }
    }

    #[test]
    fn handle_store_is_bounded_and_does_not_rebind_a_live_handle() {
        let mut store = Handles::default();
        let first = HANDLE_RANGE.start;
        for offset in 0..MAX_HANDLES as i64 {
            assert!(store.insert(first + offset, offset + 1));
        }
        assert!(!store.insert(first, 9999));
        assert_eq!(store.positions[&first], 1);
        assert!(store.insert(first + MAX_HANDLES as i64, 9999));
        assert!(!store.positions.contains_key(&first));
        assert_eq!(store.positions.len(), MAX_HANDLES);
        assert_eq!(store.issued.len(), MAX_HANDLES);
    }
}
