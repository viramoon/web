use openssl::rand::rand_bytes;
use std::{
    collections::{HashMap, VecDeque},
    sync::{LazyLock, Mutex},
};

pub(super) const FIRST_HANDLE: i64 = 1 << 52;
pub(super) const MAX_SAFE_CURSOR: i64 = (1 << 53) - 1;
const MAX_HANDLES: usize = 4096;

// Membership authenticates the handle. Its bits are independent of the scan position.
static HANDLES: LazyLock<Mutex<Handles>> = LazyLock::new(Mutex::default);

pub(super) fn issue(position: i64) -> Result<i64, ()> {
    for _ in 0..8 {
        let mut bytes = [0; 8];
        rand_bytes(&mut bytes).map_err(|_| ())?;
        let handle = FIRST_HANDLE | (i64::from_be_bytes(bytes) & (FIRST_HANDLE - 1));
        if HANDLES.lock().map_err(|_| ())?.insert(handle, position) {
            return Ok(handle);
        }
    }
    Err(())
}

pub(super) fn resolve(value: i64) -> Option<i64> {
    match value {
        0..FIRST_HANDLE => Some(value),
        FIRST_HANDLE..=MAX_SAFE_CURSOR => HANDLES.lock().ok()?.positions.get(&value).copied(),
        _ => None,
    }
}

pub(super) fn is_job_id(value: i64) -> bool {
    (1..FIRST_HANDLE).contains(&value)
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
        assert!((FIRST_HANDLE..=MAX_SAFE_CURSOR).contains(&handle));
        assert_eq!(resolve(handle), Some(208));
        assert_ne!(handle, issue(208).unwrap());
        assert_eq!(resolve(handle ^ 1), None);
        assert!(!Handles::default().positions.contains_key(&handle));
    }

    #[test]
    fn cursor_parser_keeps_legacy_nonnegative_decimal_input() {
        assert_eq!(resolve(0), Some(0));
        assert_eq!(resolve(7), Some(7));
        assert_eq!(resolve(FIRST_HANDLE - 1), Some(FIRST_HANDLE - 1));
        assert_eq!(resolve(-1), None);
        assert_eq!(resolve(MAX_SAFE_CURSOR + 1), None);
        assert_eq!(resolve(i64::MAX), None);
        assert!(!is_job_id(FIRST_HANDLE));
        assert!(!is_job_id(MAX_SAFE_CURSOR));
        assert!(is_job_id(FIRST_HANDLE - 1));
    }

    #[test]
    fn handle_store_is_bounded_and_does_not_rebind_a_live_handle() {
        let mut store = Handles::default();
        for offset in 0..MAX_HANDLES as i64 {
            assert!(store.insert(FIRST_HANDLE + offset, offset + 1));
        }
        assert!(!store.insert(FIRST_HANDLE, 9999));
        assert_eq!(store.positions[&FIRST_HANDLE], 1);
        assert!(store.insert(FIRST_HANDLE + MAX_HANDLES as i64, 9999));
        assert!(!store.positions.contains_key(&FIRST_HANDLE));
        assert_eq!(store.positions.len(), MAX_HANDLES);
        assert_eq!(store.issued.len(), MAX_HANDLES);
    }
}
